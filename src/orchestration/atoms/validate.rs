use anyhow::Result;

use super::types::{
    AtomEffects, AtomManifest, AtomProvenance, validate_description_length,
    validate_when_to_use_nonempty,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub step: &'static str,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "validation failed at {}: {}", self.step, self.message)
    }
}

impl std::error::Error for ValidationError {}

const ATOM_V1_CONTRACT: &str = "atom/v1";
const ATOM_KIND: &str = "atom";

pub const REFACTOR_SUBCONTRACT: &str = "refactor/v1";

pub const RECOGNIZED_REFACTOR_PERSONAS: &[&str] = &[
    "rust-refactor-persona",
    "rust-architecture-pathologist",
    "java-refactor-persona",
    "java-architecture-pathologist",
    "csharp-refactor-persona",
    "elixir-refactor-persona",
    "performance-pathologist",
];

const REF_PREFIX_BROFILE: &str = "brofile:";
const REF_PREFIX_WORKFLOW: &str = "workflow:";
const REF_PREFIX_ATOM: &str = "atom:";

pub fn parse_typed_ref(
    raw: &str,
    expected_prefix: &str,
) -> Result<(String, String), ValidationError> {
    let step = "lint_refs";
    if !raw.starts_with(expected_prefix) {
        return Err(ValidationError {
            step,
            message: format!("ref must start with `{expected_prefix}`, got `{raw}`"),
        });
    }
    let after_prefix = &raw[expected_prefix.len()..];
    if after_prefix.is_empty() {
        return Err(ValidationError {
            step,
            message: format!("ref `{raw}` has no name after prefix"),
        });
    }
    if let Some(at_pos) = after_prefix.find('@') {
        let name = &after_prefix[..at_pos];
        let version_part = &after_prefix[at_pos + 1..];
        if name.is_empty() {
            return Err(ValidationError {
                step,
                message: format!("ref `{raw}` has empty name before @"),
            });
        }
        if version_part != "latest"
            && version_part
                .strip_prefix('v')
                .and_then(|s| s.parse::<u32>().ok())
                .map(|n| n > 0)
                != Some(true)
        {
            return Err(ValidationError {
                step,
                message: format!(
                    "ref `{raw}` version must be @latest or @vN, got `@{version_part}`"
                ),
            });
        }
        Ok((name.to_string(), version_part.to_string()))
    } else {
        Err(ValidationError {
            step,
            message: format!(
                "stored artifact ref must include @version (e.g. `@v1` or `@latest`); bare names are rejected: `{raw}`"
            ),
        })
    }
}

pub struct InstallCtx<B: Fn(&str) -> bool, A: Fn(&str) -> bool> {
    pub brofile_exists: B,
    pub atom_exists: A,
}

pub fn validate_atom_install<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    value: &serde_json::Value,
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    if !value.is_object() {
        return Err(ValidationError {
            step: "shape",
            message: "atom artifact must be a JSON object".into(),
        });
    }

    let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != ATOM_KIND {
        return Err(ValidationError {
            step: "shape",
            message: format!("atom artifact must have `kind: \"atom\"`, got `{kind}`"),
        });
    }

    let contract = value
        .get("_contract")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if contract != ATOM_V1_CONTRACT {
        return Err(ValidationError {
            step: "shape",
            message: format!("atom artifact must have `_contract: \"atom/v1\"`, got `{contract}`"),
        });
    }

    let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err(ValidationError {
            step: "shape",
            message: "atom artifact missing required field `name`".into(),
        });
    }

    let version_val = value.get("version");
    match version_val {
        None => {
            return Err(ValidationError {
                step: "shape",
                message: "atom artifact missing required field `version`".into(),
            });
        }
        Some(v) => {
            if !v.is_u64() {
                return Err(ValidationError {
                    step: "shape",
                    message: format!(
                        "atom artifact `version` must be a positive integer, got `{v}`"
                    ),
                });
            }
            if v.as_u64() == Some(0) {
                return Err(ValidationError {
                    step: "shape",
                    message: "atom artifact `version` must be > 0".into(),
                });
            }
        }
    }

    if let Some(sub) = value.get("subcontract") {
        match sub {
            serde_json::Value::String(s) if !s.is_empty() => {}
            _ => {
                return Err(ValidationError {
                    step: "shape",
                    message: "atom artifact `subcontract` must be a non-empty string if present"
                        .into(),
                });
            }
        }
    }

    let manifest_value = value.get("manifest").ok_or_else(|| ValidationError {
        step: "shape",
        message: "atom artifact missing required field `manifest`".into(),
    })?;
    if !manifest_value.is_object() {
        return Err(ValidationError {
            step: "manifest_deserialize",
            message: "`manifest` must be a JSON object".into(),
        });
    }

    let manifest: AtomManifest =
        serde_json::from_value(manifest_value.clone()).map_err(|e| ValidationError {
            step: "manifest_deserialize",
            message: format!("failed to parse manifest: {e}"),
        })?;

    lint_manifest(&manifest, ctx)?;

    let subcontract = value
        .get("subcontract")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if subcontract == REFACTOR_SUBCONTRACT {
        validate_refactor_subcontract(&manifest)?;
    }

    Ok(())
}

fn lint_manifest<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    manifest: &AtomManifest,
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    validate_description_length(&manifest.description).map_err(|msg| ValidationError {
        step: "lint_description",
        message: msg,
    })?;

    validate_when_to_use_nonempty(&manifest.when_to_use).map_err(|msg| ValidationError {
        step: "lint_when_to_use",
        message: msg,
    })?;

    for item in &manifest.anti_patterns {
        if item.len() > 200 {
            let preview: String = item.chars().take(50).collect();
            return Err(ValidationError {
                step: "lint_anti_patterns",
                message: format!(
                    "anti_pattern item too long ({} chars, max 200): {preview}...",
                    item.len(),
                ),
            });
        }
    }

    for item in &manifest.when_to_use {
        if item.len() > 200 {
            let preview: String = item.chars().take(50).collect();
            return Err(ValidationError {
                step: "lint_when_to_use",
                message: format!(
                    "when_to_use item too long ({} chars, max 200): {preview}...",
                    item.len(),
                ),
            });
        }
    }

    if let Some(inputs) = &manifest.inputs {
        if let Some(schema) = &inputs.schema {
            validate_json_schema_ish(schema, "inputs.schema")?;
        }
        if let Some(template) = &inputs.prompt_template {
            validate_prompt_template(template, inputs.schema.as_ref())?;
        }
    }

    if let Some(outputs) = &manifest.outputs {
        if let Some(schema) = &outputs.schema {
            validate_json_schema_ish(schema, "outputs.schema")?;
        }
    }

    if let Some(effects) = &manifest.effects {
        validate_effects(effects)?;
    }

    if let Some(composition) = &manifest.composition {
        validate_composition(composition, ctx)?;
    }

    validate_implementation(&manifest.implementation, ctx)?;

    if let Some(supervision) = &manifest.supervision {
        validate_supervision(supervision, ctx)?;
    }

    validate_provenance_refs(manifest)?;

    Ok(())
}

fn validate_json_schema_ish(
    value: &serde_json::Value,
    field_name: &'static str,
) -> Result<(), ValidationError> {
    reject_remote_schema_refs(value, field_name)?;
    match value {
        serde_json::Value::Bool(_) => Ok(()),
        serde_json::Value::Object(_) => jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .compile(value)
            .map(|_| ())
            .map_err(|e| ValidationError {
                step: "lint_schema",
                message: format!("{field_name}: invalid JSON Schema 2020-12: {e}"),
            }),
        _ => Err(ValidationError {
            step: "lint_schema",
            message: format!("{field_name}: must be a JSON object or boolean, got {value}"),
        }),
    }
}

fn reject_remote_schema_refs(
    value: &serde_json::Value,
    field_name: &'static str,
) -> Result<(), ValidationError> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(reference)) = map.get("$ref") {
                if reference.starts_with("http://") || reference.starts_with("https://") {
                    return Err(ValidationError {
                        step: "lint_schema",
                        message: format!(
                            "{field_name}: remote `$ref` values are not allowed: {reference}"
                        ),
                    });
                }
            }
            for child in map.values() {
                reject_remote_schema_refs(child, field_name)?;
            }
            Ok(())
        }
        serde_json::Value::Array(items) => {
            for child in items {
                reject_remote_schema_refs(child, field_name)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_prompt_template(
    template: &str,
    schema: Option<&serde_json::Value>,
) -> Result<(), ValidationError> {
    let allowed_props = schema
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
        .map(|props| {
            props
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        });
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(ValidationError {
                step: "lint_prompt_template",
                message: "prompt_template has an unclosed `{{` placeholder".into(),
            });
        };
        let name = after[..end].trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ValidationError {
                step: "lint_prompt_template",
                message: format!(
                    "prompt_template placeholder `{name}` is not a simple property name"
                ),
            });
        }
        if let Some(allowed) = &allowed_props {
            if !allowed.contains(name) {
                return Err(ValidationError {
                    step: "lint_prompt_template",
                    message: format!(
                        "prompt_template placeholder `{name}` is not declared in inputs.schema.properties"
                    ),
                });
            }
        }
        rest = &after[end + 2..];
    }
    if rest.contains("}}") {
        return Err(ValidationError {
            step: "lint_prompt_template",
            message: "prompt_template has a closing `}}` without an opening `{{`".into(),
        });
    }
    Ok(())
}

fn validate_effects(effects: &AtomEffects) -> Result<(), ValidationError> {
    if let Some(wf) = &effects.writes_files {
        match wf {
            serde_json::Value::Bool(_) => {}
            serde_json::Value::Object(map) => {
                if !map.contains_key("scoped") {
                    return Err(ValidationError {
                        step: "lint_effects",
                        message: "effects.writes_files object must have a `scoped` key".into(),
                    });
                }
            }
            _ => {
                return Err(ValidationError {
                    step: "lint_effects",
                    message: format!(
                        "effects.writes_files must be bool or {{scoped: [...]}}, got {wf}"
                    ),
                });
            }
        }
    }
    if let Some(dr) = &effects.dispatches_runs {
        match dr {
            serde_json::Value::Number(n) => {
                if n.as_u64() != Some(0) && !n.is_u64() {
                    return Err(ValidationError {
                        step: "lint_effects",
                        message: "effects.dispatches_runs must be a non-negative integer or \"unbounded\"".into(),
                    });
                }
            }
            serde_json::Value::String(s) if s == "unbounded" => {}
            _ => {
                return Err(ValidationError {
                    step: "lint_effects",
                    message: format!(
                        "effects.dispatches_runs must be a non-negative integer or \"unbounded\", got {dr}"
                    ),
                });
            }
        }
    }
    if let Some(md) = &effects.max_depth {
        match md {
            serde_json::Value::Number(n) => {
                if !n.is_u64() {
                    return Err(ValidationError {
                        step: "lint_effects",
                        message:
                            "effects.max_depth must be a non-negative integer or \"unbounded\""
                                .into(),
                    });
                }
            }
            serde_json::Value::String(s) if s == "unbounded" => {}
            _ => {
                return Err(ValidationError {
                    step: "lint_effects",
                    message: format!(
                        "effects.max_depth must be a non-negative integer or \"unbounded\", got {md}"
                    ),
                });
            }
        }
    }
    if let Some(un) = &effects.uses_network {
        match un {
            serde_json::Value::Bool(_) => {}
            serde_json::Value::Object(map) => {
                if !map.contains_key("gated_by") {
                    return Err(ValidationError {
                        step: "lint_effects",
                        message: "effects.uses_network object must have a `gated_by` key".into(),
                    });
                }
            }
            _ => {
                return Err(ValidationError {
                    step: "lint_effects",
                    message: format!(
                        "effects.uses_network must be bool or {{gated_by: ...}}, got {un}"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_composition<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    composition: &super::types::AtomComposition,
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    match &composition.may_invoke_atoms {
        super::types::MayInvokeAtoms::Allowed { atoms } => {
            for atom_ref in atoms {
                if atom_ref.trim().is_empty() {
                    return Err(ValidationError {
                        step: "lint_composition",
                        message: "composition.may_invoke_atoms.atoms contains an empty string"
                            .into(),
                    });
                }
                let (name, _ver) = parse_typed_ref(atom_ref, REF_PREFIX_ATOM)?;
                if !(ctx.atom_exists)(name.as_str()) {
                    return Err(ValidationError {
                        step: "lint_refs",
                        message: format!(
                            "composition.may_invoke_atoms references unknown atom `{name}`"
                        ),
                    });
                }
            }
        }
        super::types::MayInvokeAtoms::None | super::types::MayInvokeAtoms::Any => {}
    }
    Ok(())
}

fn validate_implementation<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    imp: &super::types::AtomImplementation,
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    match imp {
        super::types::AtomImplementation::Profile { brofile_ref } => {
            if brofile_ref.trim().is_empty() {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: "implementation.brofile_ref must not be empty".into(),
                });
            }
            let (name, _ver) = parse_typed_ref(brofile_ref, REF_PREFIX_BROFILE)?;
            if !(ctx.brofile_exists)(name.as_str()) {
                return Err(ValidationError {
                    step: "lint_refs",
                    message: format!(
                        "implementation.brofile_ref references unknown brofile `{name}`"
                    ),
                });
            }
        }
        super::types::AtomImplementation::Workflow { workflow_ref } => {
            if workflow_ref.trim().is_empty() {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: "implementation.workflow_ref must not be empty".into(),
                });
            }
            let (_name, _ver) = parse_typed_ref(workflow_ref, REF_PREFIX_WORKFLOW)?;
        }
        super::types::AtomImplementation::Deterministic { runner } => {
            if runner.trim().is_empty() {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: "implementation.runner must not be empty".into(),
                });
            }
            let registry = super::runners::default_registry();
            if !registry.has_deterministic(runner) {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: format!("unknown deterministic atom runner `{runner}`"),
                });
            }
        }
        super::types::AtomImplementation::Adapter { adapter_name } => {
            if adapter_name.trim().is_empty() {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: "implementation.adapter_name must not be empty".into(),
                });
            }
            let registry = super::runners::default_registry();
            if !registry.has_adapter(adapter_name) {
                return Err(ValidationError {
                    step: "lint_implementation",
                    message: format!("unknown atom adapter `{adapter_name}`"),
                });
            }
        }
    }
    Ok(())
}

const ORACLE_RESERVED: &[&str] = &["none", "default"];
const ADVISOR_RESERVED: &[&str] = &["none", "on_alert", "always"];

fn validate_supervision<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    supervision: &super::types::AtomSupervisionPolicy,
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    validate_supervision_field(&supervision.oracle, "oracle", ORACLE_RESERVED, ctx)?;
    validate_supervision_field(&supervision.advisor, "advisor", ADVISOR_RESERVED, ctx)?;
    Ok(())
}

fn validate_supervision_field<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    value: &str,
    field_name: &str,
    reserved: &[&str],
    ctx: &InstallCtx<B, A>,
) -> Result<(), ValidationError> {
    if reserved.contains(&value) {
        return Ok(());
    }
    if value.starts_with(REF_PREFIX_ATOM) {
        let (name, _ver) = parse_typed_ref(value, REF_PREFIX_ATOM)?;
        if !(ctx.atom_exists)(name.as_str()) {
            return Err(ValidationError {
                step: "lint_supervision",
                message: format!("supervision.{field_name} references unknown atom `{name}`"),
            });
        }
        return Ok(());
    }
    Err(ValidationError {
        step: "lint_supervision",
        message: format!(
            "supervision.{field_name} must be one of {:?} or a typed atom ref (atom:name@vN), got `{value}`",
            reserved
        ),
    })
}

fn validate_provenance_refs(manifest: &AtomManifest) -> Result<(), ValidationError> {
    let Some(AtomProvenance::Distilled {
        evidence_session_ids,
        created_from_threads,
        ..
    }) = manifest.provenance.as_ref()
    else {
        return Ok(());
    };
    for session in evidence_session_ids {
        match crate::entity_ref::EntityRef::parse(session) {
            Ok(crate::entity_ref::EntityRef::Session { .. }) => {}
            Ok(other) => {
                return Err(ValidationError {
                    step: "lint_provenance",
                    message: format!(
                        "evidence_session_ids entry `{session}` must be a session ref, got {:?}",
                        other.entity_type()
                    ),
                });
            }
            Err(err) => {
                return Err(ValidationError {
                    step: "lint_provenance",
                    message: format!(
                        "evidence_session_ids entry `{session}` is not a canonical entity ref: {err}"
                    ),
                });
            }
        }
    }
    for thread in created_from_threads {
        match crate::entity_ref::EntityRef::parse(thread) {
            Ok(crate::entity_ref::EntityRef::Thread { .. }) => {}
            Ok(other) => {
                return Err(ValidationError {
                    step: "lint_provenance",
                    message: format!(
                        "created_from_threads entry `{thread}` must be a thread ref, got {:?}",
                        other.entity_type()
                    ),
                });
            }
            Err(err) => {
                return Err(ValidationError {
                    step: "lint_provenance",
                    message: format!(
                        "created_from_threads entry `{thread}` is not a canonical entity ref: {err}"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_refactor_subcontract(manifest: &AtomManifest) -> Result<(), ValidationError> {
    match &manifest.implementation {
        super::types::AtomImplementation::Profile { brofile_ref } => {
            let (name, _ver) =
                parse_typed_ref(brofile_ref, REF_PREFIX_BROFILE).map_err(|e| ValidationError {
                    step: "refactor_subcontract",
                    message: format!("refactor/v1 subcontract: {e}"),
                })?;
            let persona_ok = RECOGNIZED_REFACTOR_PERSONAS
                .iter()
                .any(|persona| *persona == name);
            if !persona_ok {
                return Err(ValidationError {
                    step: "refactor_subcontract",
                    message: format!(
                        "refactor/v1 subcontract: implementation.brofile_ref must reference one of {:?}; got `{brofile_ref}`",
                        RECOGNIZED_REFACTOR_PERSONAS
                    ),
                });
            }
        }
        _ => {
            return Err(ValidationError {
                step: "refactor_subcontract",
                message: "refactor/v1 subcontract requires implementation.kind=\"profile\"".into(),
            });
        }
    }

    if let Some(props) = manifest
        .inputs
        .as_ref()
        .and_then(|i| i.schema.as_ref())
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
    {
        for (name, prop_schema) in props {
            if name.starts_with("acknowledge_") && prop_schema.get("default").is_some() {
                return Err(ValidationError {
                    step: "refactor_subcontract",
                    message: format!(
                        "refactor/v1 subcontract: inputs.schema.properties.{name} declares \
                         a `default`; operator-authority opt-outs must be explicit — drop the default."
                    ),
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_toml_front_matter(raw: &str) -> &str {
        let mut parts = raw.splitn(3, "+++");
        parts.next();
        parts.next();
        match parts.next() {
            Some(body) => body
                .strip_prefix('\n')
                .or_else(|| body.strip_prefix("\r\n"))
                .unwrap_or(body),
            None => raw,
        }
    }

    fn noop_ctx() -> InstallCtx<impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        InstallCtx {
            brofile_exists: |_| true,
            atom_exists: |_| true,
        }
    }

    fn ctx_with_unknown_brofile() -> InstallCtx<impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        InstallCtx {
            brofile_exists: |_| false,
            atom_exists: |_| true,
        }
    }

    fn ctx_with_unknown_atom() -> InstallCtx<impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        InstallCtx {
            brofile_exists: |_| true,
            atom_exists: |_| false,
        }
    }

    fn minimal_valid_atom() -> serde_json::Value {
        serde_json::json!({
            "_contract": "atom/v1",
            "kind": "atom",
            "name": "test-atom",
            "version": 1,
            "manifest": {
                "description": "A test atom for validation.",
                "when_to_use": ["when testing"],
                "implementation": {
                    "kind": "profile",
                    "brofile_ref": "brofile:reviewer@v1"
                }
            }
        })
    }

    #[test]
    fn valid_minimal_atom_passes() {
        validate_atom_install(&minimal_valid_atom(), &noop_ctx()).unwrap();
    }

    #[test]
    fn rejects_wrong_contract() {
        let mut v = minimal_valid_atom();
        v["_contract"] = serde_json::json!("agent/v1");
        let err = validate_atom_install(&v, &noop_ctx()).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("atom/v1"));
    }

    #[test]
    fn rejects_wrong_kind() {
        let mut v = minimal_valid_atom();
        v["kind"] = serde_json::json!("agent");
        let err = validate_atom_install(&v, &noop_ctx()).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("kind"));
    }

    #[test]
    fn rejects_missing_name() {
        let mut v = minimal_valid_atom();
        v.as_object_mut().unwrap().remove("name");
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("name"));
    }

    #[test]
    fn rejects_missing_version() {
        let mut v = minimal_valid_atom();
        v.as_object_mut().unwrap().remove("version");
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("version"));
    }

    #[test]
    fn rejects_version_zero() {
        let mut v = minimal_valid_atom();
        v["version"] = serde_json::json!(0);
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("> 0"));
    }

    #[test]
    fn rejects_missing_manifest() {
        let mut v = minimal_valid_atom();
        v.as_object_mut().unwrap().remove("manifest");
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("manifest"));
    }

    #[test]
    fn rejects_non_object_manifest() {
        let mut v = minimal_valid_atom();
        v["manifest"] = serde_json::json!("not an object");
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "manifest_deserialize");
    }

    #[test]
    fn rejects_short_description() {
        let mut v = minimal_valid_atom();
        v["manifest"]["description"] = serde_json::json!("short");
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_description");
    }

    #[test]
    fn rejects_empty_when_to_use() {
        let mut v = minimal_valid_atom();
        v["manifest"]["when_to_use"] = serde_json::json!([]);
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_when_to_use");
    }

    #[test]
    fn accepts_workflow_implementation() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "workflow",
            "workflow_ref": "workflow:review-arc@v1"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_deterministic_implementation() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "deterministic",
            "runner": "refactor-plan-validate"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_adapter_implementation() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "adapter",
            "adapter_name": "badgey"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn rejects_empty_brofile_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": ""
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_implementation");
    }

    #[test]
    fn accepts_valid_effects() {
        let mut v = minimal_valid_atom();
        v["manifest"]["effects"] = serde_json::json!({
            "writes_files": false,
            "dispatches_runs": 2,
            "max_depth": 1,
            "uses_network": false
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_scoped_writes_files() {
        let mut v = minimal_valid_atom();
        v["manifest"]["effects"] = serde_json::json!({
            "writes_files": { "scoped": ["src/**/*.rs"] }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_unbounded_dispatches() {
        let mut v = minimal_valid_atom();
        v["manifest"]["effects"] = serde_json::json!({
            "dispatches_runs": "unbounded"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_gated_network() {
        let mut v = minimal_valid_atom();
        v["manifest"]["effects"] = serde_json::json!({
            "uses_network": { "gated_by": "packet:network-policy@v1" }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_composition_none() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "none" }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_composition_allowed() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["atom:reviewer@v1"] }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_composition_any() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "any" }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn refactor_subcontract_requires_profile_with_persona() {
        let mut v = minimal_valid_atom();
        v["subcontract"] = serde_json::json!("refactor/v1");
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:rust-refactor-persona@v1"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn refactor_subcontract_rejects_wrong_persona() {
        let mut v = minimal_valid_atom();
        v["subcontract"] = serde_json::json!("refactor/v1");
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:some-other-persona@v1"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "refactor_subcontract");
    }

    #[test]
    fn refactor_subcontract_rejects_workflow_impl() {
        let mut v = minimal_valid_atom();
        v["subcontract"] = serde_json::json!("refactor/v1");
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "workflow",
            "workflow_ref": "workflow:x@v1"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "refactor_subcontract");
        assert!(err.message.contains("profile"));
    }

    #[test]
    fn refactor_subcontract_rejects_acknowledge_default() {
        let mut v = minimal_valid_atom();
        v["subcontract"] = serde_json::json!("refactor/v1");
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:rust-refactor-persona@v1"
        });
        v["manifest"]["inputs"] = serde_json::json!({
            "schema": {
                "type": "object",
                "properties": {
                    "acknowledge_risk": { "type": "boolean", "default": true }
                }
            }
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "refactor_subcontract");
        assert!(err.message.contains("acknowledge_risk"));
    }

    fn validate_atom_install_helper(v: &serde_json::Value) -> ValidationError {
        validate_atom_install(v, &noop_ctx()).unwrap_err()
    }

    // ── Typed-ref validation tests ──────────────────────────────

    #[test]
    fn rejects_bare_brofile_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "rust-refactor-persona"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("must start with `brofile:`"));
    }

    #[test]
    fn rejects_bare_brofile_ref_without_version() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:reviewer"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("@version"));
    }

    #[test]
    fn rejects_unknown_brofile_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:no-such-persona@v1"
        });
        let err = validate_atom_install(&v, &ctx_with_unknown_brofile()).unwrap_err();
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("unknown brofile `no-such-persona`"));
    }

    #[test]
    fn rejects_bare_workflow_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "workflow",
            "workflow_ref": "review-arc"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("must start with `workflow:`"));
    }

    #[test]
    fn rejects_bare_allowed_atom_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["some-atom@v1"] }
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("must start with `atom:`"));
    }

    #[test]
    fn rejects_bare_allowed_atom_ref_without_version() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["atom:some-atom"] }
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("@version"));
    }

    #[test]
    fn rejects_unknown_allowed_atom_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["atom:no-such-atom@v1"] }
        });
        let err = validate_atom_install(&v, &ctx_with_unknown_atom()).unwrap_err();
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("unknown atom `no-such-atom`"));
    }

    #[test]
    fn accepts_known_allowed_atom_ref() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["atom:known-atom@v1"] }
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn refactor_subcontract_passes_with_typed_brofile_ref() {
        let mut v = minimal_valid_atom();
        v["subcontract"] = serde_json::json!("refactor/v1");
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:rust-refactor-persona@v1"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    // ── @v0 rejection tests ─────────────────────────────────────

    #[test]
    fn rejects_brofile_ref_v0() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "profile",
            "brofile_ref": "brofile:reviewer@v0"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("@v0") || err.message.contains("@latest or @vN"));
    }

    #[test]
    fn rejects_atom_ref_v0() {
        let mut v = minimal_valid_atom();
        v["manifest"]["composition"] = serde_json::json!({
            "may_invoke_atoms": { "kind": "allowed", "atoms": ["atom:some-atom@v0"] }
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("@v0") || err.message.contains("@latest or @vN"));
    }

    #[test]
    fn rejects_workflow_ref_v0() {
        let mut v = minimal_valid_atom();
        v["manifest"]["implementation"] = serde_json::json!({
            "kind": "workflow",
            "workflow_ref": "workflow:some-workflow@v0"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_refs");
        assert!(err.message.contains("@v0") || err.message.contains("@latest or @vN"));
    }

    // ── Supervision validation tests ─────────────────────────────

    #[test]
    fn accepts_supervision_reserved_oracle() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "default",
            "advisor": "none"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_supervision_reserved_advisor() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "none",
            "advisor": "on_alert"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn accepts_supervision_atom_ref_oracle() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "atom:oracle-atom@v1",
            "advisor": "none"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    #[test]
    fn rejects_supervision_bare_oracle() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "some-name",
            "advisor": "none"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_supervision");
        assert!(err.message.contains("oracle"));
    }

    #[test]
    fn rejects_supervision_bare_advisor() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "none",
            "advisor": "some-advisor"
        });
        let err = validate_atom_install_helper(&v);
        assert_eq!(err.step, "lint_supervision");
        assert!(err.message.contains("advisor"));
    }

    #[test]
    fn rejects_supervision_unknown_atom_ref_oracle() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "atom:no-such@v1",
            "advisor": "none"
        });
        let err = validate_atom_install(&v, &ctx_with_unknown_atom()).unwrap_err();
        assert_eq!(err.step, "lint_supervision");
        assert!(err.message.contains("unknown atom"));
    }

    #[test]
    fn accepts_supervision_always_advisor() {
        let mut v = minimal_valid_atom();
        v["manifest"]["supervision"] = serde_json::json!({
            "oracle": "none",
            "advisor": "always"
        });
        validate_atom_install(&v, &noop_ctx()).unwrap();
    }

    fn shipped_refactor_atom_names() -> Vec<String> {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/atoms/refactor");
        let mut atom_names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("refactor atoms dir exists") {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            if filename.starts_with('_') {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read manifest");
            let v: serde_json::Value = serde_json::from_str(&src).expect("manifest parses");
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_else(|| panic!("{filename}: missing top-level name"))
                .to_string();
            atom_names.push(name);
        }
        atom_names.sort();
        atom_names
    }

    #[test]
    fn every_shipped_refactor_atom_passes_atom_validation() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/atoms/refactor");
        let entries = std::fs::read_dir(&dir).expect("refactor atoms dir exists");
        let mut found = 0usize;
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with('_') {
                continue;
            }
            found += 1;
            let src = std::fs::read_to_string(&path).expect("read manifest");
            let v: serde_json::Value =
                serde_json::from_str(&src).unwrap_or_else(|e| panic!("{name} did not parse: {e}"));
            assert_eq!(
                v.get("_contract").and_then(|v| v.as_str()),
                Some("atom/v1"),
                "{name} must use atom/v1 contract"
            );
            assert_eq!(
                v.get("kind").and_then(|v| v.as_str()),
                Some("atom"),
                "{name} must be kind=atom"
            );
            assert_eq!(
                v.get("subcontract").and_then(|v| v.as_str()),
                Some(REFACTOR_SUBCONTRACT),
                "{name} must opt into refactor/v1 subcontract"
            );
            validate_atom_install(&v, &noop_ctx())
                .unwrap_or_else(|e| panic!("{name} failed atom validation: {e}"));
        }
        assert!(
            found >= 1,
            "expected at least one shipped refactor atom under system-defaults/atoms/refactor/"
        );
    }

    #[test]
    fn java_refactor_plan_kinds_have_atom_coverage() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/atoms/refactor");
        let mut atom_text = String::new();
        for entry in std::fs::read_dir(&dir).expect("refactor atoms dir exists") {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let filename = path.file_name().unwrap().to_string_lossy();
            if filename.starts_with('_') || !filename.starts_with("java-") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read manifest");
            let v: serde_json::Value = serde_json::from_str(&src).expect("manifest parses");
            if let Some(template) = v
                .pointer("/manifest/inputs/prompt_template")
                .and_then(|value| value.as_str())
            {
                atom_text.push_str(template);
                atom_text.push('\n');
            }
        }

        let java_plan_kinds = [
            "extract_java_class",
            "extract_java_methods",
            "extract_java_nested_classes",
            "extract_java_interface",
            "promote_java_inner_class",
            "add_java_fields",
            "add_java_constructor",
            "add_java_delegate_field",
            "add_java_implements",
            "move_java_field",
            "move_java_constant",
            "update_java_callers",
            "rewrite_java_visibility",
            "rename_java_symbol",
            "migrate_java_type_usages",
            "find_java_usages",
            "java_class_dependency_analysis",
            "extract_java_class_cohesive_clusters",
            "cluster_inject_params_java",
            "java_concurrency_antipattern_audit",
            "java_public_api_guard",
            "java_lsp_organize_imports",
            "prune_java_orphans",
            "extract_java_code_block_to_method",
            "convert_method_to_class",
            "inline_java_class",
            "inline_java_method",
            "extract_java_test_slice",
            "java_collapse_call_chain",
            "migrate_java_method_receiver",
            "java_split_provider",
            "replace_java_static_reference",
            "java_vaadin_extract_component",
            "java_vaadin_extract_grid_component",
            "java_vaadin_extract_dialog_class",
            "java_vaadin_synthesize_view",
            "java_vaadin_register_route_access",
            "java_vaadin_navigation_helper_extract",
            "java_vaadin_route_inventory",
            "java_vaadin_static_ui_context_audit",
            "java_vaadin_view_structure_analysis",
            "java_jooq_codegen_config_inventory",
            "java_jooq_query_structure_analysis",
            "java_jooq_dsl_context_audit",
            "java_jooq_projection_mapping_analysis",
            "java_jooq_raw_sql_fragment_audit",
            "java_jooq_raw_sql_fragment_rewrite",
            "java_jooq_extract_query_method",
            "java_jooq_extract_query_object",
            "java_jooq_extract_repository",
            "java_jooq_synthesize_repository",
            "java_jooq_synthesize_query_method",
            "java_jooq_synthesize_dto_projection",
            "java_jooq_condition_builder_extract",
            "java_jooq_transaction_boundary_normalize",
            "java_jooq_generated_record_boundary_analysis",
            "java_jooq_generated_record_boundary_rewrite",
            "java_jooq_codegen_extension_apply",
        ];

        for kind in java_plan_kinds {
            assert!(
                atom_text.contains(&format!("kind=\"{kind}\""))
                    || atom_text.contains(&format!("kind=\\\"{kind}\\\""))
                    || atom_text.contains(&format!("bbox_refactor_plan:{kind}")),
                "missing Java refactor atom coverage for plan kind `{kind}`"
            );
        }
    }

    #[test]
    fn sm_refactor_uses_signposts_not_atom_ledger() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(crate_root.join("system-defaults/memories/refactor.md"))
            .expect("read sm-refactor from system-defaults");
        let catalog = strip_toml_front_matter(&raw);

        assert!(
            catalog.contains("atom_search(query=\"<intent phrase>\")")
                && catalog.contains("System memory is not the atom catalog"),
            "sm-refactor should signpost atom discovery, not mirror the atom catalog"
        );
        assert!(
            !catalog.contains("| Atom | Status | Cost |")
                && !catalog.contains("shipped (v")
                && !catalog.contains("RA-A")
                && !catalog.contains("RA-X"),
            "sm-refactor must not become a historical atom ledger; \
             shipped atom coverage belongs to manifest/eval tests"
        );
    }

    #[test]
    fn refactor_atom_eval_suites_cover_every_shipped_atom() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let atom_names = shipped_refactor_atom_names();

        let eval_dir = crate_root.join("eval/atoms/refactor");
        let discovery: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(eval_dir.join("discovery-queries.json"))
                .expect("read discovery-queries.json"),
        )
        .expect("discovery-queries.json parses");
        let dispatch: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(eval_dir.join("dispatch-scenarios.json"))
                .expect("read dispatch-scenarios.json"),
        )
        .expect("dispatch-scenarios.json parses");
        let behavior: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(eval_dir.join("behavior-smoke.json"))
                .expect("read behavior-smoke.json"),
        )
        .expect("behavior-smoke.json parses");

        let discovery_atoms: std::collections::HashSet<String> = discovery["queries"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|q| q["expected_atoms"].as_array().unwrap().iter())
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let dispatch_atoms: std::collections::HashSet<String> = dispatch["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["atom"].as_str().unwrap().to_string())
            .collect();
        let behavior_atoms: std::collections::HashSet<String> = behavior["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["atom"].as_str().unwrap().to_string())
            .collect();

        for name in &atom_names {
            assert!(
                discovery_atoms.contains(name),
                "eval/atoms/refactor/discovery-queries.json missing {name}"
            );
            assert!(
                dispatch_atoms.contains(name),
                "eval/atoms/refactor/dispatch-scenarios.json missing {name}"
            );
            assert!(
                behavior_atoms.contains(name),
                "eval/atoms/refactor/behavior-smoke.json missing {name}"
            );
        }
    }

    #[test]
    fn refactor_atom_reference_workflows_parse_and_compile() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/workflows/refactor");
        let entries = std::fs::read_dir(&dir).expect("refactor workflows dir exists");
        let mut found = 0usize;
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            found += 1;
            let src = std::fs::read_to_string(&path).expect("read workflow");
            let spec: crate::workflow::Workflow = serde_json::from_str(&src)
                .unwrap_or_else(|e| panic!("{name} did not parse as Workflow: {e}"));
            crate::workflow::compile(spec)
                .unwrap_or_else(|e| panic!("{name} failed to compile: {e}"));
        }
        assert!(
            found >= 1,
            "expected at least one reference workflow under system-defaults/workflows/refactor/"
        );
    }

    #[test]
    fn supervision_atom_workflows_parse_and_compile() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/workflows/supervision");
        let entries = std::fs::read_dir(&dir).expect("supervision workflows dir exists");
        let mut found = 0usize;
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            found += 1;
            let src = std::fs::read_to_string(&path).expect("read workflow");
            let spec: crate::workflow::Workflow = serde_json::from_str(&src)
                .unwrap_or_else(|e| panic!("{name} did not parse as Workflow: {e}"));
            crate::workflow::compile(spec)
                .unwrap_or_else(|e| panic!("{name} failed to compile: {e}"));
        }
        assert!(
            found >= 1,
            "expected at least one supervision workflow under system-defaults/workflows/supervision/"
        );
    }

    #[test]
    fn every_shipped_atom_artifact_passes_validation() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = crate_root.join("system-defaults/atoms");
        let entries = walkdir::WalkDir::new(&dir);
        let mut found = 0usize;
        for entry in entries.into_iter().filter_map(|entry| entry.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.starts_with('_') {
                continue;
            }
            let src = std::fs::read_to_string(path).expect("read manifest");
            let v: serde_json::Value =
                serde_json::from_str(&src).unwrap_or_else(|e| panic!("{name} did not parse: {e}"));
            assert!(
                v.get("kind").and_then(|v| v.as_str()) == Some("atom"),
                "{name} under system-defaults/atoms/ must have kind=\"atom\""
            );
            found += 1;
            validate_atom_install(&v, &noop_ctx())
                .unwrap_or_else(|e| panic!("{name} failed atom validation: {e}"));
        }
        assert!(
            found >= 1,
            "expected at least one shipped atom artifact under system-defaults/atoms/"
        );
    }

    // ── Phase 7 artifact validation ─────────────────────────────

    #[test]
    fn forgejo_ensure_user_atom_artifact_parses_and_validates() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/forgejo/atoms/forgejo-ensure-user.json"),
        )
        .expect("examples/forgejo/atoms/forgejo-ensure-user.json exists");
        let v: serde_json::Value =
            serde_json::from_str(&src).expect("forgejo-ensure-user.json parses as JSON");
        assert_eq!(
            v.get("_contract").and_then(|v| v.as_str()),
            Some("atom/v1"),
            "must use atom/v1 contract"
        );
        assert_eq!(
            v.get("kind").and_then(|v| v.as_str()),
            Some("atom"),
            "must be kind=atom"
        );
        assert_eq!(
            v.get("name").and_then(|v| v.as_str()),
            Some("forgejo-ensure-user"),
            "name must match"
        );
        let impl_kind = v
            .pointer("/manifest/implementation/kind")
            .and_then(|v| v.as_str());
        assert_eq!(
            impl_kind,
            Some("deterministic"),
            "implementation must be deterministic (builtin path handled by system_events::executors)"
        );
        let runner = v
            .pointer("/manifest/implementation/runner")
            .and_then(|v| v.as_str());
        assert_eq!(
            runner,
            Some("forgejo-ensure-user"),
            "runner must be forgejo-ensure-user"
        );
        validate_atom_install(&v, &noop_ctx())
            .expect("forgejo-ensure-user.json must pass atom validation");
    }

    #[test]
    fn forgejo_reaction_example_has_atom_invoke_for_builtin() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/forgejo/reactions/ensure-bro-user.json"),
        )
        .expect("examples/forgejo/reactions/ensure-bro-user.json exists");
        let v: serde_json::Value =
            serde_json::from_str(&src).expect("ensure-bro-user.json parses as JSON");
        let op = v.pointer("/action/op").and_then(|v| v.as_str());
        assert_eq!(
            op,
            Some("atom_invoke"),
            "reaction action op must be atom_invoke"
        );
        let atom = v.pointer("/action/args/atom").and_then(|v| v.as_str());
        assert_eq!(
            atom,
            Some("atom:forgejo-ensure-user@v1"),
            "reaction must invoke the forgejo built-in atom ref"
        );
    }
}

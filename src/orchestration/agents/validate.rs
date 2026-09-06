use anyhow::Result;

use super::adapter::AgentAdapterRegistry;
use super::types::{
    AgentFilterOverlay, AgentManifest, AgentProvenance, validate_description_length,
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

fn json_type_label(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

pub struct InstallCtx<'a, B: Fn(&str) -> bool, A: Fn(&str) -> bool> {
    pub adapter_registry: &'a AgentAdapterRegistry,
    pub brofile_exists: B,
    pub agent_exists: A,
}

pub fn validate_agent_install<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    value: &serde_json::Value,
    ctx: &InstallCtx<'_, B, A>,
) -> Result<(), ValidationError> {
    if !value.is_object() {
        return Err(ValidationError {
            step: "shape",
            message: "agent artifact must be a JSON object".into(),
        });
    }

    let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "agent" {
        return Err(ValidationError {
            step: "shape",
            message: format!("agent artifact must have `kind: \"agent\"`, got `{kind}`"),
        });
    }

    let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err(ValidationError {
            step: "shape",
            message: "agent artifact missing required field `name`".into(),
        });
    }

    let version_val = value.get("version");
    match version_val {
        None => {
            return Err(ValidationError {
                step: "shape",
                message: "agent artifact missing required field `version`".into(),
            });
        }
        Some(v) => {
            if !v.is_u64() {
                return Err(ValidationError {
                    step: "shape",
                    message: format!(
                        "agent artifact `version` must be a positive integer, got `{v}`"
                    ),
                });
            }
            if v.as_u64() == Some(0) {
                return Err(ValidationError {
                    step: "shape",
                    message: "agent artifact `version` must be > 0".into(),
                });
            }
        }
    }

    if let Some(supersedes_val) = value.get("supersedes") {
        match supersedes_val {
            serde_json::Value::String(s) if !s.is_empty() => {}
            serde_json::Value::String(_) => {
                return Err(ValidationError {
                    step: "shape",
                    message: "agent artifact `supersedes` must be a non-empty string if present"
                        .into(),
                });
            }
            other => {
                return Err(ValidationError {
                    step: "shape",
                    message: format!(
                        "agent artifact `supersedes` must be a string, got {}",
                        json_type_label(other)
                    ),
                });
            }
        }
    }

    let manifest_value = value.get("manifest").ok_or_else(|| ValidationError {
        step: "shape",
        message: "agent artifact missing required field `manifest` (canonical wrapper required)"
            .into(),
    })?;
    if !manifest_value.is_object() {
        return Err(ValidationError {
            step: "manifest_deserialize",
            message: "`manifest` must be a JSON object".into(),
        });
    }

    let manifest: AgentManifest =
        serde_json::from_value(manifest_value.clone()).map_err(|e| ValidationError {
            step: "manifest_deserialize",
            message: format!("failed to parse manifest: {e}"),
        })?;

    validate_brofile_xor(&manifest)?;

    if let Some(brofile_ref) = &manifest.brofile_ref {
        if !(ctx.brofile_exists)(brofile_ref) {
            return Err(ValidationError {
                step: "brofile_resolution",
                message: format!("brofile_ref `{brofile_ref}` not found in catalog"),
            });
        }
    }

    lint_manifest(&manifest, ctx)?;
    validate_agent_artifact_schema(value)?;

    Ok(())
}

fn validate_agent_artifact_schema(value: &serde_json::Value) -> Result<(), ValidationError> {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../schema/agent.schema.json")).map_err(|e| {
            ValidationError {
                step: "schema",
                message: format!("shipped agent.schema.json is invalid JSON: {e}"),
            }
        })?;
    let compiled = jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .compile(&schema)
        .map_err(|e| ValidationError {
            step: "schema",
            message: format!("shipped agent.schema.json failed to compile: {e}"),
        })?;
    if let Err(errors) = compiled.validate(value) {
        return Err(ValidationError {
            step: "schema",
            message: errors.map(|e| e.to_string()).collect::<Vec<_>>().join("; "),
        });
    }
    Ok(())
}

fn validate_brofile_xor(manifest: &AgentManifest) -> Result<(), ValidationError> {
    let has_ref = manifest.brofile_ref.is_some();
    let has_inline = manifest.brofile_inline.is_some();
    match (has_ref, has_inline) {
        (true, true) => Err(ValidationError {
            step: "brofile_xor",
            message: "manifest must specify exactly one of brofile_ref or brofile_inline, not both"
                .into(),
        }),
        (false, false) => Err(ValidationError {
            step: "brofile_xor",
            message: "manifest must specify exactly one of brofile_ref or brofile_inline".into(),
        }),
        _ => Ok(()),
    }
}

fn lint_manifest<B: Fn(&str) -> bool, A: Fn(&str) -> bool>(
    manifest: &AgentManifest,
    ctx: &InstallCtx<'_, B, A>,
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

    lint_filter_overlay(&manifest.filter_overlay)?;

    if let Some(adapter_name) = &manifest.dispatch_adapter {
        if ctx.adapter_registry.get(adapter_name).is_none() {
            return Err(ValidationError {
                step: "lint_dispatch_adapter",
                message: format!(
                    "dispatch_adapter `{adapter_name}` is not registered in the adapter registry"
                ),
            });
        }
    }

    if let Some(composition) = &manifest.composition {
        validate_composition(composition, &ctx.agent_exists)?;
    }

    validate_provenance_refs(manifest)?;

    Ok(())
}

fn validate_provenance_refs(manifest: &AgentManifest) -> Result<(), ValidationError> {
    let Some(AgentProvenance::Distilled {
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

fn validate_prompt_template(
    template: &str,
    schema: Option<&serde_json::Value>,
) -> Result<(), ValidationError> {
    let allowed_props = schema
        .and_then(|schema| schema.get("properties"))
        .and_then(|props| props.as_object())
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

const FAN_OUT_AGGREGATOR_VARIANTS: &[&str] = &["vote-majority", "ensemble-merge", "first-success"];

fn validate_composition(
    composition: &super::types::AgentComposition,
    agent_exists: &impl Fn(&str) -> bool,
) -> Result<(), ValidationError> {
    for name in &composition.chainable_after {
        if name.is_empty() {
            return Err(ValidationError {
                step: "lint_composition",
                message: "composition.chainable_after contains an empty string".into(),
            });
        }
        if name.trim() != name {
            return Err(ValidationError {
                step: "lint_composition",
                message: format!(
                    "composition.chainable_after entry `{name}` must not have leading or trailing whitespace"
                ),
            });
        }
        if !agent_exists(name) {
            tracing::warn!(
                agent = name.as_str(),
                "composition.chainable_after forward reference recorded during agent validation"
            );
        }
    }

    if let Some(ref aggregator) = composition.fan_out_aggregator {
        let is_builtin = FAN_OUT_AGGREGATOR_VARIANTS.contains(&aggregator.as_str());
        let is_well_formed_node_id = !aggregator.is_empty()
            && aggregator
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ':');
        if !is_builtin && !is_well_formed_node_id {
            return Err(ValidationError {
                step: "lint_composition",
                message: format!(
                    "composition.fan_out_aggregator must be one of {:?} or a well-formed node identifier (alphanumeric, '-', '_', ':'), got `{aggregator}`",
                    FAN_OUT_AGGREGATOR_VARIANTS
                ),
            });
        }
    }

    Ok(())
}

fn validate_json_schema_ish(
    value: &serde_json::Value,
    field_name: &'static str,
) -> Result<(), ValidationError> {
    reject_remote_schema_refs(value, field_name)?;
    match value {
        serde_json::Value::Bool(_) => Ok(()),
        serde_json::Value::Object(map) => {
            if let Some(t) = map.get("type") {
                if !t.is_string() && !t.is_array() {
                    return Err(ValidationError {
                        step: "lint_schema",
                        message: format!(
                            "{field_name}: `type` must be a string or array, got {}",
                            t
                        ),
                    });
                }
            }
            jsonschema::JSONSchema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .compile(value)
                .map(|_| ())
                .map_err(|e| ValidationError {
                    step: "lint_schema",
                    message: format!("{field_name}: invalid JSON Schema 2020-12: {e}"),
                })
        }
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
                            "{field_name}: remote `$ref` values are not allowed during agent install: {reference}"
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

fn lint_filter_overlay(overlay: &Option<AgentFilterOverlay>) -> Result<(), ValidationError> {
    let Some(ov) = overlay else {
        return Ok(());
    };
    let all_patterns: Vec<&str> = ov
        .allow
        .iter()
        .chain(ov.disallow.iter())
        .map(|s| s.as_str())
        .collect();
    for pat in &all_patterns {
        if pat.trim().is_empty() {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: "filter pattern must not be empty or whitespace".into(),
            });
        }
        if pat.starts_with('-') {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: format!("filter pattern must not start with '-': {pat}"),
            });
        }
        if pat.contains(' ') {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: format!("filter pattern must not contain spaces: {pat}"),
            });
        }
    }
    let allow_set: Vec<&str> = ov.allow.iter().map(|s| s.as_str()).collect();
    let disallow_set: Vec<&str> = ov.disallow.iter().map(|s| s.as_str()).collect();
    for a in &allow_set {
        if disallow_set.contains(a) {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: format!("filter overlay has same pattern in allow and disallow: {a}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct NoopAdapter;

    impl super::super::adapter::AgentDispatchAdapter for NoopAdapter {
        fn name(&self) -> &'static str {
            "noop"
        }

        fn dispatch(
            &self,
            _manifest: &AgentManifest,
            _args: serde_json::Value,
            _ctx: super::super::adapter::DispatchContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            super::super::adapter::AgentDispatchResult,
                            super::super::adapter::AgentDispatchError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Ok(super::super::adapter::AgentDispatchResult {
                    session: super::super::types::AgentSession {
                        session_id: "noop".into(),
                        provider: "test".into(),
                        project_dir: None,
                        agent: super::super::types::AgentRef {
                            name: "noop".into(),
                            version: 1,
                        },
                        task_id: None,
                    },
                    resolved_brofile: None,
                    merged_filters: Default::default(),
                    degraded: None,
                })
            })
        }
    }

    fn make_ctx(
        registry: &AgentAdapterRegistry,
    ) -> InstallCtx<'_, impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        InstallCtx {
            adapter_registry: registry,
            brofile_exists: |_name: &str| true,
            agent_exists: |_name: &str| true,
        }
    }

    fn make_ctx_brofile_missing(
        registry: &AgentAdapterRegistry,
    ) -> InstallCtx<'_, impl Fn(&str) -> bool, impl Fn(&str) -> bool> {
        InstallCtx {
            adapter_registry: registry,
            brofile_exists: |_name: &str| false,
            agent_exists: |_name: &str| true,
        }
    }

    fn minimal_valid_agent() -> serde_json::Value {
        serde_json::json!({
            "kind": "agent",
            "name": "test-agent",
            "version": 1,
            "manifest": {
                "description": "A test agent for validation.",
                "when_to_use": ["when testing"],
                "brofile_inline": {"provider": "claude", "lens": "reviewer"}
            }
        })
    }

    #[test]
    fn valid_minimal_agent_passes() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        validate_agent_install(&minimal_valid_agent(), &ctx).unwrap();
    }

    #[test]
    fn shipped_agent_schema_validates_reference_agents() {
        let schema_raw = include_str!("../../../schema/agent.schema.json");
        let schema: serde_json::Value =
            serde_json::from_str(schema_raw).expect("agent schema must be valid JSON");
        let compiled = jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .compile(&schema)
            .expect("agent schema must compile as draft 2020-12");
        for (name, raw) in [
            (
                "code-reviewer",
                include_str!("../../../system-defaults/agents/code-reviewer.json"),
            ),
            (
                "diff-narrator",
                include_str!("../../../system-defaults/agents/diff-narrator.json"),
            ),
            (
                "badgey",
                include_str!("../../../tests/fixtures/retired-orchestration/badgey-agent.json"),
            ),
            (
                "corpus-pathfinder",
                include_str!("../../../system-defaults/agents/corpus-pathfinder.json"),
            ),
        ] {
            let value: serde_json::Value =
                serde_json::from_str(raw).expect("reference agent must be valid JSON");
            assert!(
                compiled.is_valid(&value),
                "{name} rejected by schema: {:?}",
                compiled
                    .validate(&value)
                    .err()
                    .map(|errs| errs.map(|e| e.to_string()).collect::<Vec<_>>())
            );
        }
    }

    #[test]
    fn install_validation_enforces_shipped_agent_schema() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["unexpected"] = serde_json::json!("nope");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "schema");
        assert!(
            err.message.contains("unexpected"),
            "schema error should name the extra property: {err}"
        );
    }

    #[test]
    fn accepts_canonical_distilled_provenance_refs() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["provenance"] = serde_json::json!({
            "kind": "distilled",
            "distilled_by": "badgey-01",
            "evidence_session_ids": ["session:claude:sess-1"],
            "created_from_threads": ["thread:thread-abc"],
            "accept_count": 1,
            "reject_count": 0
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_noncanonical_distilled_provenance_refs() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["provenance"] = serde_json::json!({
            "kind": "distilled",
            "distilled_by": "badgey-01",
            "evidence_session_ids": ["sess-1"],
            "created_from_threads": ["thread-abc"],
            "accept_count": 1,
            "reject_count": 0
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_provenance");
    }

    #[test]
    fn rejects_non_object() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let err = validate_agent_install(&serde_json::json!("string"), &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
    }

    #[test]
    fn rejects_missing_name() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v.as_object_mut().unwrap().remove("name");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("name"));
    }

    #[test]
    fn rejects_missing_version() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v.as_object_mut().unwrap().remove("version");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("version"));
    }

    #[test]
    fn rejects_version_zero() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["version"] = serde_json::json!(0);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("> 0"));
    }

    #[test]
    fn rejects_bad_manifest_json() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"] = serde_json::json!("not an object");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "manifest_deserialize");
    }

    #[test]
    fn rejects_both_brofile_ref_and_inline() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["brofile_ref"] = serde_json::json!("some-ref");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "brofile_xor");
    }

    #[test]
    fn rejects_neither_brofile_ref_nor_inline() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]
            .as_object_mut()
            .unwrap()
            .remove("brofile_inline");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "brofile_xor");
    }

    #[test]
    fn rejects_unknown_brofile_ref() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx_brofile_missing(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]
            .as_object_mut()
            .unwrap()
            .remove("brofile_inline");
        v["manifest"]["brofile_ref"] = serde_json::json!("missing-brofile");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "brofile_resolution");
    }

    #[test]
    fn rejects_short_description() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["description"] = serde_json::json!("short");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_description");
    }

    #[test]
    fn rejects_empty_when_to_use() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["when_to_use"] = serde_json::json!([]);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_when_to_use");
    }

    #[test]
    fn rejects_anti_pattern_too_long() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["anti_patterns"] = serde_json::json!(["x".repeat(201)]);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_anti_patterns");
    }

    #[test]
    fn rejects_when_to_use_item_too_long() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["when_to_use"] = serde_json::json!(["x".repeat(201)]);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_when_to_use");
    }

    #[test]
    fn rejects_invalid_input_schema_type() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["inputs"] = serde_json::json!({
            "schema": {"type": 42}
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_schema");
    }

    #[test]
    fn rejects_remote_schema_refs() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["inputs"] = serde_json::json!({
            "schema": {
                "type": "object",
                "properties": {
                    "payload": {"$ref": "https://example.com/schema.json"}
                }
            }
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_schema");
        assert!(err.message.contains("remote `$ref`"));
    }

    #[test]
    fn accepts_bool_input_schema() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["inputs"] = serde_json::json!({"schema": true});
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_empty_filter_pattern() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["filter_overlay"] = serde_json::json!({
            "allow": [""],
            "disallow": []
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_filter_overlay");
    }

    #[test]
    fn rejects_duplicate_allow_disallow() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["filter_overlay"] = serde_json::json!({
            "allow": ["mcp__blackbox__bbox_search"],
            "disallow": ["mcp__blackbox__bbox_search"]
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_filter_overlay");
    }

    #[test]
    fn rejects_unregistered_dispatch_adapter() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["dispatch_adapter"] = serde_json::json!("nonexistent");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_dispatch_adapter");
    }

    #[test]
    fn accepts_registered_dispatch_adapter() {
        let mut registry = AgentAdapterRegistry::new();
        registry.register(Arc::new(NoopAdapter));
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["dispatch_adapter"] = serde_json::json!("noop");
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn valid_full_agent_passes() {
        let mut registry = AgentAdapterRegistry::new();
        registry.register(Arc::new(NoopAdapter));
        let ctx = make_ctx(&registry);
        let v = serde_json::json!({
            "kind": "agent",
            "name": "full-reviewer",
            "version": 2,
            "supersedes": "full-reviewer",
            "manifest": {
                "description": "Full-featured code review agent for testing.",
                "when_to_use": ["after code changes", "on pull request"],
                "anti_patterns": ["one-line typo fixes"],
                "brofile_inline": {"provider": "claude"},
                "filter_overlay": {
                    "allow": ["mcp__blackbox__bbox_*"],
                    "disallow": ["mcp__blackbox__bro_exec"]
                },
                "inputs": {
                    "schema": {"type": "object", "properties": {"diff": {"type": "string"}}},
                    "prompt_template": "Review: {{diff}}"
                },
                "outputs": {
                    "schema": {"type": "object"},
                    "evidence_density": "high"
                },
                "composition": {
                    "chainable_after": ["test-writer"],
                    "parallel_safe": true,
                    "fan_out_aggregator": "vote-majority"
                },
                "cost_class": "normal",
                "dispatch_adapter": "noop",
                "provenance": {
                    "kind": "hand_authored",
                    "author": "test"
                }
            }
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_missing_manifest_key() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let v = serde_json::json!({
            "kind": "agent",
            "name": "flat-agent",
            "version": 1,
            "description": "A flat agent without nested manifest.",
            "when_to_use": ["when testing"],
            "brofile_inline": {"provider": "claude"}
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("manifest"));
    }

    #[test]
    fn rejects_wrong_kind() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["kind"] = serde_json::json!("packet");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("kind"));
    }

    #[test]
    fn rejects_missing_kind() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v.as_object_mut().unwrap().remove("kind");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("kind"));
    }

    #[test]
    fn rejects_float_version() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["version"] = serde_json::json!(1.5);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("positive integer"));
    }

    #[test]
    fn rejects_negative_version() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["version"] = serde_json::json!(-1);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("positive integer"));
    }

    #[test]
    fn rejects_empty_supersedes() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["supersedes"] = serde_json::json!("");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("supersedes"));
    }

    #[test]
    fn accepts_valid_supersedes() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["supersedes"] = serde_json::json!("previous-version");
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_non_string_supersedes() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["supersedes"] = serde_json::json!(42);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("supersedes"));
        assert!(err.message.contains("string"));
    }

    #[test]
    fn rejects_boolean_supersedes() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["supersedes"] = serde_json::json!(true);
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "shape");
        assert!(err.message.contains("supersedes"));
    }

    #[test]
    fn rejects_filter_pattern_with_spaces() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["filter_overlay"] = serde_json::json!({
            "allow": ["not a pattern"],
            "disallow": []
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_filter_overlay");
        assert!(err.message.contains("spaces"));
    }

    #[test]
    fn rejects_filter_pattern_starting_with_dash() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["filter_overlay"] = serde_json::json!({
            "allow": ["-bad-pattern"],
            "disallow": []
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_filter_overlay");
        assert!(err.message.contains("-"));
    }

    #[test]
    fn accepts_valid_composition() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "chainable_after": ["analyzer"],
            "parallel_safe": true,
            "fan_out_aggregator": "vote-majority",
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn accepts_empty_chainable_after() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "chainable_after": [],
            "parallel_safe": false,
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn accepts_chainable_after_forward_reference() {
        let registry = AgentAdapterRegistry::new();
        let ctx = InstallCtx {
            adapter_registry: &registry,
            brofile_exists: |_name: &str| true,
            agent_exists: |_name: &str| false,
        };
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "chainable_after": ["future-agent"],
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_empty_chainable_after_entry() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "chainable_after": [""],
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_composition");
        assert!(err.message.contains("empty string"));
    }

    #[test]
    fn rejects_whitespace_padded_chainable_after_entry() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "chainable_after": [" analyzer"],
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_composition");
        assert!(err.message.contains("whitespace"));
    }

    #[test]
    fn rejects_invalid_fan_out_aggregator() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "fan_out_aggregator": "has spaces in it",
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_composition");
        assert!(err.message.contains("fan_out_aggregator"));
        assert!(err.message.contains("well-formed"));
    }

    #[test]
    fn accepts_custom_node_id_as_aggregator() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "fan_out_aggregator": "my-custom-aggregator-node",
        });
        validate_agent_install(&v, &ctx).unwrap();
    }

    #[test]
    fn rejects_empty_aggregator() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let mut v = minimal_valid_agent();
        v["manifest"]["composition"] = serde_json::json!({
            "fan_out_aggregator": "",
        });
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_composition");
        assert!(err.message.contains("fan_out_aggregator"));
    }

    #[test]
    fn accepts_all_valid_aggregator_variants() {
        for variant in &["vote-majority", "ensemble-merge", "first-success"] {
            let registry = AgentAdapterRegistry::new();
            let ctx = make_ctx(&registry);
            let mut v = minimal_valid_agent();
            v["manifest"]["composition"] = serde_json::json!({
                "fan_out_aggregator": variant,
            });
            validate_agent_install(&v, &ctx).unwrap();
        }
    }

    struct NamedAdapter(&'static str);

    impl super::super::adapter::AgentDispatchAdapter for NamedAdapter {
        fn name(&self) -> &'static str {
            self.0
        }

        fn dispatch(
            &self,
            _manifest: &AgentManifest,
            _args: serde_json::Value,
            _ctx: super::super::adapter::DispatchContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            super::super::adapter::AgentDispatchResult,
                            super::super::adapter::AgentDispatchError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async {
                Err(super::super::adapter::AgentDispatchError::AdapterFailed {
                    message: "not implemented".into(),
                })
            })
        }
    }

    #[test]
    fn reference_agent_diff_narrator_installs_cleanly() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let src = include_str!("../../../system-defaults/agents/diff-narrator.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("diff-narrator.json parses");
        validate_agent_install(&v, &ctx).expect("diff-narrator validates cleanly");
    }

    #[test]
    fn reference_agent_code_reviewer_installs_with_brofile() {
        let registry = AgentAdapterRegistry::new();
        let ctx = InstallCtx {
            adapter_registry: &registry,
            brofile_exists: |name: &str| name == "code-reviewer-persona",
            agent_exists: |name: &str| name == "diff-narrator",
        };
        let src = include_str!("../../../system-defaults/agents/code-reviewer.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("code-reviewer.json parses");
        validate_agent_install(&v, &ctx).expect("code-reviewer validates cleanly");
    }

    #[test]
    fn reference_agent_code_reviewer_fails_without_brofile() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx_brofile_missing(&registry);
        let src = include_str!("../../../system-defaults/agents/code-reviewer.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("code-reviewer.json parses");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "brofile_resolution");
    }

    #[test]
    fn reference_agent_badgey_installs_with_adapter() {
        let mut registry = AgentAdapterRegistry::new();
        registry.register(Arc::new(NamedAdapter("badgey")));
        let ctx = InstallCtx {
            adapter_registry: &registry,
            brofile_exists: |_name: &str| true,
            agent_exists: |_name: &str| true,
        };
        let src = include_str!("../../../tests/fixtures/retired-orchestration/badgey-agent.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("badgey.json parses");
        validate_agent_install(&v, &ctx).expect("badgey validates cleanly");
    }

    #[test]
    fn reference_agent_badgey_fails_without_adapter() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let src = include_str!("../../../tests/fixtures/retired-orchestration/badgey-agent.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("badgey.json parses");
        let err = validate_agent_install(&v, &ctx).unwrap_err();
        assert_eq!(err.step, "lint_dispatch_adapter");
    }

    #[test]
    fn reference_agents_install_via_artifact_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = crate::artifacts::ArtifactCatalog::open(tmp.path()).unwrap();

        let persona_src =
            include_str!("../../../system-defaults/brofiles/code-reviewer-persona.json");
        let persona_v: serde_json::Value =
            serde_json::from_str(persona_src).expect("persona parses");
        catalog
            .install_value(
                crate::artifacts::ArtifactKind::Brofile,
                "code-reviewer-persona.json".into(),
                &persona_v,
                Some("code-reviewer-persona".into()),
                None,
                None,
            )
            .expect("brofile install");

        let pathfinder_src = include_str!(
            "../../../system-defaults/brofiles/phase-decompose/corpus-pathfinder.json"
        );
        let pathfinder_v: serde_json::Value =
            serde_json::from_str(pathfinder_src).expect("corpus-pathfinder brofile parses");
        catalog
            .install_value(
                crate::artifacts::ArtifactKind::Brofile,
                "corpus-pathfinder.json".into(),
                &pathfinder_v,
                Some("corpus-pathfinder".into()),
                None,
                None,
            )
            .expect("corpus-pathfinder brofile install");

        let mut registry = AgentAdapterRegistry::new();
        struct TestBadgeyAdapter;
        impl super::super::adapter::AgentDispatchAdapter for TestBadgeyAdapter {
            fn name(&self) -> &'static str {
                "badgey"
            }
            fn dispatch(
                &self,
                _manifest: &AgentManifest,
                _args: serde_json::Value,
                _ctx: super::super::adapter::DispatchContext,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                super::super::adapter::AgentDispatchResult,
                                super::super::adapter::AgentDispatchError,
                            >,
                        > + Send,
                >,
            > {
                Box::pin(async {
                    Err(super::super::adapter::AgentDispatchError::AdapterFailed {
                        message: "stub".into(),
                    })
                })
            }
        }
        registry.register(Arc::new(TestBadgeyAdapter));

        let agents: &[&str] = &[
            include_str!("../../../system-defaults/agents/code-reviewer.json"),
            include_str!("../../../system-defaults/agents/diff-narrator.json"),
            include_str!("../../../tests/fixtures/retired-orchestration/badgey-agent.json"),
            include_str!("../../../system-defaults/agents/corpus-pathfinder.json"),
        ];
        for src in agents {
            let v: serde_json::Value = serde_json::from_str(src).expect("agent JSON parses");
            let ctx = InstallCtx {
                adapter_registry: &registry,
                brofile_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(crate::artifacts::ArtifactKind::Brofile, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
                agent_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(crate::artifacts::ArtifactKind::Agent, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
            };
            let agent_name = v["name"].as_str().unwrap_or("unknown");
            validate_agent_install(&v, &ctx)
                .unwrap_or_else(|e| panic!("{agent_name} validation failed: {e}"));
            catalog
                .install_value(
                    crate::artifacts::ArtifactKind::Agent,
                    format!("{agent_name}.json"),
                    &v,
                    None,
                    None,
                    None,
                )
                .unwrap_or_else(|e| panic!("{agent_name} artifact install failed: {e}"));
        }

        let rows = catalog
            .list(&crate::artifacts::ArtifactListParams {
                kind: Some(crate::artifacts::ArtifactKind::Agent),
                name: None,
                include_superseded: false,
            })
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(
            names.contains(&"code-reviewer"),
            "code-reviewer not in catalog"
        );
        assert!(
            names.contains(&"diff-narrator"),
            "diff-narrator not in catalog"
        );
        assert!(names.contains(&"badgey"), "badgey not in catalog");
        assert!(
            names.contains(&"corpus-pathfinder"),
            "corpus-pathfinder not in catalog"
        );
    }

    #[test]
    fn badgey_adapter_contract_inline_brofile_is_documentation() {
        let mut registry = AgentAdapterRegistry::new();
        struct TestBadgeyAdapter;
        impl super::super::adapter::AgentDispatchAdapter for TestBadgeyAdapter {
            fn name(&self) -> &'static str {
                "badgey"
            }
            fn dispatch(
                &self,
                _manifest: &AgentManifest,
                _args: serde_json::Value,
                _ctx: super::super::adapter::DispatchContext,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                super::super::adapter::AgentDispatchResult,
                                super::super::adapter::AgentDispatchError,
                            >,
                        > + Send,
                >,
            > {
                Box::pin(async {
                    Err(super::super::adapter::AgentDispatchError::AdapterFailed {
                        message: "stub".into(),
                    })
                })
            }
        }
        registry.register(Arc::new(TestBadgeyAdapter));

        let src = include_str!("../../../tests/fixtures/retired-orchestration/badgey-agent.json");
        let v: serde_json::Value = serde_json::from_str(src).expect("badgey.json parses");
        let ctx = make_ctx(&registry);
        validate_agent_install(&v, &ctx).expect("badgey with registered adapter should validate");

        let manifest = serde_json::from_value::<AgentManifest>(v["manifest"].clone())
            .expect("manifest deserializes");
        assert!(
            manifest.brofile_inline.is_some(),
            "badgey declares inline brofile for documentation/identity"
        );
        assert_eq!(
            manifest.dispatch_adapter.as_deref(),
            Some("badgey"),
            "dispatch_adapter: badgey owns lifecycle; inline brofile serves as documentation"
        );
    }

    #[test]
    fn reference_agents_dispatch_via_noop_adapter_returns_expected_handles() {
        let mut registry = AgentAdapterRegistry::new();
        struct DispatchingNoopAdapter;
        impl super::super::adapter::AgentDispatchAdapter for DispatchingNoopAdapter {
            fn name(&self) -> &'static str {
                "noop-dispatch"
            }
            fn dispatch(
                &self,
                manifest: &AgentManifest,
                args: serde_json::Value,
                ctx: super::super::adapter::DispatchContext,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                super::super::adapter::AgentDispatchResult,
                                super::super::adapter::AgentDispatchError,
                            >,
                        > + Send,
                >,
            > {
                let name = manifest.description.clone();
                let args_string = args.to_string();
                Box::pin(async move {
                    Ok(super::super::adapter::AgentDispatchResult {
                        session: super::super::types::AgentSession {
                            session_id: format!("stub-session-{}", name),
                            provider: "stub".into(),
                            project_dir: ctx.project_dir,
                            agent: super::super::types::AgentRef {
                                name: name.clone(),
                                version: 1,
                            },
                            task_id: Some(format!("stub-task-{}", name)),
                        },
                        resolved_brofile: None,
                        merged_filters: Default::default(),
                        degraded: if args_string.len() > 1000 {
                            Some(super::super::adapter::DispatchDegraded {
                                reasons: vec!["args_too_large_for_stub".into()],
                            })
                        } else {
                            None
                        },
                    })
                })
            }
        }
        registry.register(Arc::new(DispatchingNoopAdapter));

        for (agent_name, agent_src) in [
            (
                "code-reviewer",
                include_str!("../../../system-defaults/agents/code-reviewer.json"),
            ),
            (
                "diff-narrator",
                include_str!("../../../system-defaults/agents/diff-narrator.json"),
            ),
        ] {
            let v: serde_json::Value = serde_json::from_str(agent_src).expect("agent parses");
            let manifest = serde_json::from_value::<AgentManifest>(v["manifest"].clone())
                .expect("manifest deserializes");

            let test_args =
                serde_json::json!({"diff": "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new"});
            let ctx = super::super::adapter::DispatchContext {
                project_dir: Some("/tmp/test-project".into()),
                ambient: None,
                bro_label_prefix: None,
                caller_provider: None,
                caller_session_id: None,
            };
            let adapter = registry.get("noop-dispatch").expect("adapter registered");
            let result = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(adapter.dispatch(&manifest, test_args, ctx))
                .unwrap_or_else(|e| panic!("{agent_name}: dispatch failed: {e}"));

            assert!(
                !result.session.session_id.is_empty(),
                "{agent_name}: session_id should be non-empty"
            );
            assert!(
                result.session.task_id.is_some(),
                "{agent_name}: task_id should be Some"
            );
            assert!(
                result.degraded.is_none(),
                "{agent_name}: should not be degraded for small args"
            );
        }
    }
}

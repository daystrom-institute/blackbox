use anyhow::{bail, Result};

use super::adapter::AgentAdapterRegistry;
use super::types::{
    validate_description_length, validate_when_to_use_nonempty, AgentFilterOverlay, AgentManifest,
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

pub struct InstallCtx<'a, F: Fn(&str) -> bool> {
    pub adapter_registry: &'a AgentAdapterRegistry,
    pub brofile_exists: F,
}

pub fn validate_agent_install<F: Fn(&str) -> bool>(
    value: &serde_json::Value,
    ctx: &InstallCtx<'_, F>,
) -> Result<(), ValidationError> {
    if !value.is_object() {
        return Err(ValidationError {
            step: "shape",
            message: "agent artifact must be a JSON object".into(),
        });
    }

    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if name.is_empty() {
        return Err(ValidationError {
            step: "shape",
            message: "agent artifact missing required field `name`".into(),
        });
    }

    let version = value.get("version");
    if version.is_none_or(|v| !v.is_number()) {
        return Err(ValidationError {
            step: "shape",
            message: "agent artifact missing or invalid `version` (must be a number)".into(),
        });
    }
    if let Some(v) = version.and_then(|v| v.as_u64()) {
        if v == 0 {
            return Err(ValidationError {
                step: "shape",
                message: "agent artifact `version` must be > 0".into(),
            });
        }
    }

    let manifest_value = value.get("manifest");
    let manifest_data = match manifest_value {
        Some(v) => v,
        None => value,
    };

    let manifest: AgentManifest = serde_json::from_value(manifest_data.clone()).map_err(|e| {
        ValidationError {
            step: "manifest_deserialize",
            message: format!("failed to parse manifest: {e}"),
        }
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

fn lint_manifest<F: Fn(&str) -> bool>(
    manifest: &AgentManifest,
    ctx: &InstallCtx<'_, F>,
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
            return Err(ValidationError {
                step: "lint_anti_patterns",
                message: format!(
                    "anti_pattern item too long ({} chars, max 200): {}...",
                    item.len(),
                    &item[..50]
                ),
            });
        }
    }

    for item in &manifest.when_to_use {
        if item.len() > 200 {
            return Err(ValidationError {
                step: "lint_when_to_use",
                message: format!(
                    "when_to_use item too long ({} chars, max 200): {}...",
                    item.len(),
                    &item[..50]
                ),
            });
        }
    }

    if let Some(inputs) = &manifest.inputs {
        if let Some(schema) = &inputs.schema {
            validate_json_schema_ish(schema, "inputs.schema")?;
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

    Ok(())
}

fn validate_json_schema_ish(
    value: &serde_json::Value,
    field_name: &'static str,
) -> Result<(), ValidationError> {
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
            Ok(())
        }
        _ => Err(ValidationError {
            step: "lint_schema",
            message: format!("{field_name}: must be a JSON object or boolean, got {value}"),
        }),
    }
}

fn lint_filter_overlay(overlay: &Option<AgentFilterOverlay>) -> Result<(), ValidationError> {
    let Some(ov) = overlay else {
        return Ok(());
    };
    let all_patterns: Vec<&str> = ov.allow.iter().chain(ov.disallow.iter()).map(|s| s.as_str()).collect();
    for pat in &all_patterns {
        if pat.is_empty() {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: "filter pattern must not be empty".into(),
            });
        }
        if pat.starts_with('-') {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: format!("filter pattern must not start with '-': {pat}"),
            });
        }
    }
    let allow_set: Vec<&str> = ov.allow.iter().map(|s| s.as_str()).collect();
    let disallow_set: Vec<&str> = ov.disallow.iter().map(|s| s.as_str()).collect();
    for a in &allow_set {
        if disallow_set.contains(a) {
            return Err(ValidationError {
                step: "lint_filter_overlay",
                message: format!(
                    "filter overlay has same pattern in allow and disallow: {a}"
                ),
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
        ) -> Result<super::super::adapter::AgentDispatchResult, super::super::adapter::AgentDispatchError>
        {
            Ok(super::super::adapter::AgentDispatchResult {
                session_id: "noop".into(),
                task_id: "noop".into(),
                resolved_brofile: None,
                merged_filters: Default::default(),
                degraded: false,
            })
        }
    }

    fn make_ctx(registry: &AgentAdapterRegistry) -> InstallCtx<'_, impl Fn(&str) -> bool> {
        InstallCtx {
            adapter_registry: registry,
            brofile_exists: |_name: &str| true,
        }
    }

    fn make_ctx_brofile_missing(registry: &AgentAdapterRegistry) -> InstallCtx<'_, impl Fn(&str) -> bool> {
        InstallCtx {
            adapter_registry: registry,
            brofile_exists: |_name: &str| false,
        }
    }

    fn minimal_valid_agent() -> serde_json::Value {
        serde_json::json!({
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
    fn manifest_at_top_level_when_no_manifest_key() {
        let registry = AgentAdapterRegistry::new();
        let ctx = make_ctx(&registry);
        let v = serde_json::json!({
            "name": "flat-agent",
            "version": 1,
            "description": "A flat agent without nested manifest.",
            "when_to_use": ["when testing"],
            "brofile_inline": {"provider": "claude"}
        });
        validate_agent_install(&v, &ctx).unwrap();
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
}

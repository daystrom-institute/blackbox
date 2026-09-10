//! Shared admission and argument authority for tools that compose other tools.

use crate::{Tool, ToolCx, ToolResult};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Remove tools whose required capabilities are absent from the admitted set.
/// Run after permission/transport filtering and before publishing any catalog.
/// Repeat to a fixed point so a removed dependency also removes its dependents.
pub fn prune_tool_dependencies(mut tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    loop {
        let names: HashSet<String> = tools.iter().map(|tool| tool.name().to_owned()).collect();
        let before = tools.len();
        tools.retain(|tool| {
            tool.required_tools()
                .iter()
                .all(|name| names.contains(*name))
        });
        if tools.len() == before {
            return tools;
        }
    }
}

/// Apply host defaults and pins before invoking a tool, retaining their rider.
/// Admission belongs to the caller's catalog; this helper adds no dispatch lock,
/// so an admitted wrapper can invoke its dependency without recursive locking.
pub async fn call_tool_with_arg_defaults(
    tool: &dyn Tool,
    name: &str,
    input: Value,
    cx: &ToolCx,
) -> ToolResult {
    call_tool_with_arg_defaults_and_fallbacks(tool, name, input, cx, &[]).await
}

/// Wrapper implementation defaults fill absent fields only after host policy
/// evaluates the authored arguments. Pins on absent input remain a no-op, just
/// as they are for a direct invocation; explicit values (including null) remain.
pub async fn call_tool_with_arg_defaults_and_fallbacks(
    tool: &dyn Tool,
    name: &str,
    input: Value,
    cx: &ToolCx,
    fallbacks: &[(&str, Value)],
) -> ToolResult {
    if cx.cancellation.is_cancelled() {
        return ToolResult::Error("tool invocation cancelled before execution".into());
    }
    if !input.is_object() && !(input.is_string() && tool.freeform_grammar().is_some()) {
        return ToolResult::Error("invalid tool arguments: expected a JSON object; no defaults applied and nothing executed".into());
    }
    let (mut input, rider) = match cx.tool_arg_defaults.apply_schema(
        name,
        input,
        &tool.input_schema(),
        tool.authority_grants(),
    ) {
        Ok(applied) => applied,
        Err(error) => {
            cx.tool_observations
                .record(name, cx.instruction_generation, error.observation());
            return error.into_tool_result(name);
        }
    };
    cx.tool_observations
        .record(name, cx.instruction_generation, rider.to_value());
    if let Some(object) = input.as_object_mut() {
        for (key, value) in fallbacks {
            object
                .entry((*key).to_owned())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(policy) = &cx.instruction_policy {
        let request = match tool.instruction_paths(&input, cx) {
            Ok(request) => request,
            Err(error) => return error,
        };
        if let Some(request) = request
            && let Err(error) = policy.check(request, cx.instruction_generation).await
        {
            return error;
        }
    }
    tool.call(input, cx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dependent(&'static str, &'static [&'static str]);

    #[async_trait::async_trait]
    impl Tool for Dependent {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Dependency fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        fn required_tools(&self) -> &[&str] {
            self.1
        }
        async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
            unreachable!()
        }
    }

    #[test]
    fn missing_dependency_prunes_transitive_dependents_without_reordering_survivors() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Dependent("outer", &["inner"])),
            Arc::new(Dependent("keep", &[])),
            Arc::new(Dependent("inner", &["missing"])),
            Arc::new(Dependent("last", &["keep"])),
        ];
        let admitted = prune_tool_dependencies(tools);
        assert_eq!(
            admitted.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["keep", "last"]
        );
    }

    fn cx(root: &std::path::Path) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 9,
            instruction_policy: None,
            root: root.to_path_buf(),
            cancellation: Default::default(),
            output_budget: 1024,
            safety: Arc::new(crate::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Default::default(),
            shell_sessions: Default::default(),
            edits: Default::default(),
            session_env: Default::default(),
            child_env: Default::default(),
            shell_env: Default::default(),
            tool_arg_defaults: Arc::new(
                crate::ToolArgDefaults::parse_values(std::collections::BTreeMap::from([
                    ("default:fixture.enabled".into(), serde_json::json!(true)),
                    ("pin:fixture.enabled".into(), serde_json::json!(true)),
                ]))
                .unwrap(),
            ),
        }
    }

    struct ExactResult(ToolResult);
    #[async_trait::async_trait]
    impl Tool for ExactResult {
        fn name(&self) -> &str {
            "fixture"
        }
        fn description(&self) -> &str {
            "Exact domain result fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"properties":{"enabled":{"type":"boolean"}}})
        }
        async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
            assert_eq!(input, serde_json::json!({"enabled":true}));
            assert_eq!(
                cx.output_budget, 1024,
                "host observation must not take domain output budget"
            );
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn host_policy_preserves_every_domain_result_shape_and_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let cx = cx(&root);
        for domain in [
            ToolResult::Text("exact source\n".into()),
            ToolResult::Json(serde_json::json!("edit-set-id")),
            ToolResult::Json(serde_json::json!([1, 2, 3])),
            ToolResult::Json(serde_json::json!({"defaults_applied":"domain value", "ok":true})),
            ToolResult::Error("domain failure\n".into()),
        ] {
            let expected = domain.clone().into_content();
            let tool = ExactResult(domain);
            let result =
                call_tool_with_arg_defaults(&tool, "fixture", serde_json::json!({}), &cx).await;
            assert_eq!(result.into_content(), expected);
            let observations = cx.tool_observations.drain();
            assert_eq!(observations.observations.len(), 1);
            assert_eq!(observations.observations[0].instruction_generation, 9);
            assert_eq!(
                observations.observations[0].context["defaults_applied"]["enabled"],
                true
            );
            assert_eq!(
                observations.observations[0].context["pin_enforced"]["enabled"],
                true
            );
        }
    }

    #[tokio::test]
    async fn invalid_host_value_refuses_before_tool_effects() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut cx = cx(&root);
        cx.tool_arg_defaults = Arc::new(
            crate::ToolArgDefaults::parse_map(std::collections::BTreeMap::from([(
                "default:fixture.enabled".into(),
                "true".into(),
            )]))
            .unwrap(),
        );
        let result = call_tool_with_arg_defaults(
            &ExactResult(ToolResult::Text("never".into())),
            "fixture",
            serde_json::json!({}),
            &cx,
        )
        .await;
        assert!(result.is_error());
        assert!(
            cx.tool_observations.drain().observations[0]
                .context
                .get("policy_error")
                .is_some()
        );
    }
}

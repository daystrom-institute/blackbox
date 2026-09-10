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
    let (mut input, rider) = match cx.tool_arg_defaults.apply(name, input) {
        Ok(applied) => applied,
        Err(conflict) => return conflict.into_tool_result(name),
    };
    if let Some(object) = input.as_object_mut() {
        for (key, value) in fallbacks {
            object
                .entry((*key).to_owned())
                .or_insert_with(|| value.clone());
        }
    }
    let result = tool.call(input, cx).await;
    crate::apply_rider(result, &rider)
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
}

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
    if cx.output_budget != 0
        && !rider.is_empty()
        && matches!(name, "shell_run" | "shell_poll" | "shell_kill")
    {
        // Reserve telemetry before a shell removes bytes from its output queue.
        // JSON object merging adds one comma and removes the rider's braces.
        let fields = shell_rider_fields(&rider, cx.output_budget);
        let overhead = fields.to_string().len().saturating_sub(1);
        let mut tool_cx = cx.clone();
        tool_cx.output_budget = cx.output_budget.saturating_sub(overhead).max(1);
        let result = tool.call(input, &tool_cx).await;
        match result {
            ToolResult::Json(Value::Object(mut object)) => {
                object.extend(fields.as_object().unwrap().clone());
                return ToolResult::Json(Value::Object(object));
            }
            ToolResult::Error(error) => {
                return ToolResult::Error(format!("{error}\n\n{fields}"));
            }
            result => return crate::apply_rider(result, &rider),
        }
    }
    let result = tool.call(input, cx).await;
    crate::apply_rider(result, &rider)
}

fn shell_rider_fields(rider: &crate::ToolArgRider, budget: usize) -> Value {
    let fields = rider.to_value();
    if fields.to_string().len().saturating_sub(1) <= (budget / 4).min(1024) {
        return fields;
    }
    // Applying a large host value is valid; repeating it is optional telemetry.
    // Preserve evidence of the policy action without consuming the output page.
    serde_json::json!({"tool_arg_context": {
        "defaults_applied_count": rider.defaults_applied.len(),
        "pins_enforced_count": rider.pin_enforced.len(),
        "values_omitted": rider.defaults_applied.len() + rider.pin_enforced.len() + rider.pin_conflict.len(),
    }})
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

    struct ShellReceipt {
        expected_marker: String,
    }

    #[async_trait::async_trait]
    impl Tool for ShellReceipt {
        fn name(&self) -> &str {
            "shell_run"
        }
        fn description(&self) -> &str {
            "Shell receipt budget fixture"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
            assert_eq!(input["marker"], self.expected_marker);
            let mut receipt = serde_json::json!({
                "exit_code":0,"running":false,"session_id":"fixture",
                "stdout":"\n\"\\日".repeat(12),"stderr":"","output_pending":true,
            });
            let size = receipt.to_string().len();
            assert!(size < cx.output_budget);
            let stdout = format!(
                "{}{}",
                receipt["stdout"].as_str().unwrap(),
                "x".repeat(cx.output_budget - size)
            );
            receipt["stdout"] = Value::String(stdout);
            assert_eq!(receipt.to_string().len(), cx.output_budget);
            ToolResult::Json(receipt)
        }
    }

    #[tokio::test]
    async fn shell_rider_is_budgeted_before_output_page_consumption() {
        for budget in [1024, 16 * 1024] {
            for marker in ["ordinary".to_string(), "\n\"\\日".repeat(10_000)] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().canonicalize().unwrap();
                let cx = ToolCx {
                    root,
                    output_budget: budget,
                    cancellation: Default::default(),
                    safety: Arc::new(crate::SafetyPolicy::new()),
                    http: reqwest::Client::new(),
                    todos: Arc::new(std::sync::Mutex::new(Default::default())),
                    shell_sessions: Arc::new(std::sync::Mutex::new(Default::default())),
                    edits: Arc::new(std::sync::Mutex::new(Default::default())),
                    session_env: Arc::new(Default::default()),
                    child_env: Arc::new(Default::default()),
                    shell_env: Arc::new(Default::default()),
                    tool_arg_defaults: Arc::new(
                        crate::ToolArgDefaults::parse_map(std::collections::BTreeMap::from([(
                            "default:shell_run.marker".into(),
                            marker.clone(),
                        )]))
                        .unwrap(),
                    ),
                };
                let tool = ShellReceipt {
                    expected_marker: marker.clone(),
                };
                let result =
                    call_tool_with_arg_defaults(&tool, "shell_run", serde_json::json!({}), &cx)
                        .await;
                let ToolResult::Json(receipt) = result else {
                    panic!("expected receipt")
                };
                assert_eq!(receipt.to_string().len(), budget);
                assert!(
                    receipt["stdout"]
                        .as_str()
                        .unwrap()
                        .starts_with(&"\n\"\\日".repeat(12))
                );
                assert_eq!(receipt["output_pending"], true);
                assert_eq!(receipt["session_id"], "fixture");
                if marker == "ordinary" {
                    assert_eq!(receipt["defaults_applied"]["marker"], marker);
                } else {
                    assert_eq!(receipt["tool_arg_context"]["defaults_applied_count"], 1);
                    assert_eq!(receipt["tool_arg_context"]["values_omitted"], 1);
                }
            }
        }
    }
}

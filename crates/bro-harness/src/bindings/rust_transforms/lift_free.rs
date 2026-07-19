//! `rust.liftToFree` - lift inherent impl methods to free functions.
//!
//! Port of v1 `lift_rust_inherent_to_free` from
//! `bbox_refactor::rust_lift_free` (design section 3.1). Thin adapter over
//! `bbox_refactor::plan`: it runs the v1 analysis/synthesis verbatim, strips
//! the MCP/plan-apply envelope, and returns `{changes, creates, findings}` for
//! the edits algebra. NEVER writes.

use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::RefactorPlanParams;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers::{
    PlanProjection, done_hint, plan_to_changes_creates, record_in_ledger, resolve_workspace_file,
};
use crate::bindings::ledger::ProvenanceLedger;

/// `rust.liftToFree` - lift selected inherent methods to free functions.
pub struct RustLiftToFree(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiftToFreeInput {
    /// Source file path (workspace-relative, no `..`).
    source: String,
    /// Target free-function file path (workspace-relative, no `..`).
    target: String,
    /// Method names to lift.
    item_names: Vec<String>,
}

#[async_trait]
impl Tool for RustLiftToFree {
    fn name(&self) -> &str {
        "rust.liftToFree"
    }

    fn description(&self) -> &str {
        "Lift selected Rust inherent impl methods that do not depend on instance state into free functions. Preserves explicit lifetimes and reports per-method refusals. NEVER writes: returns {changes, creates, findings} for edits.merge/createFile."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (workspace-relative, no `..`)." },
                "target": { "type": "string", "description": "Target free-function file path (workspace-relative, no `..`). May be a new file." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Inherent method names to lift." }
            },
            "required": ["source", "target", "itemNames"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "liftToFree".to_string()))
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: LiftToFreeInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::Error(format!("invalid input: {error}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("itemNames is required and must not be empty".to_string());
        }

        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || Self::run(params, &root, &ledger)).await
    }
}

impl RustLiftToFree {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker.
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: LiftToFreeInput,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source_abs = match resolve_workspace_file(root, &params.source, "rust.liftToFree") {
            Ok(path) => path,
            Err(error) => return ToolResult::Error(format!("rust.liftToFree: {error}")),
        };
        let target_abs = match resolve_workspace_file(root, &params.target, "rust.liftToFree") {
            Ok(path) => path,
            Err(error) => return ToolResult::Error(format!("rust.liftToFree: {error}")),
        };
        let plan_params = RefactorPlanParams {
            kind: "lift_rust_inherent_to_free".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: Some(target_abs.to_string_lossy().into_owned()),
            item_names: Some(params.item_names),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan_params) {
            Ok(plan) => plan,
            Err(error) => {
                let message = format!("{error:#}");
                let hint = done_hint(&message);
                return ToolResult::Error(format!("rust.liftToFree: {message}{hint}"));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::Error(format!("rust.liftToFree: plan decode: {error}"));
            }
        };
        let file_edits: Vec<bbox_refactor::FileEdit> =
            match serde_json::from_value(plan_value["edits"].clone()) {
                Ok(edits) => edits,
                Err(error) => {
                    return ToolResult::Error(format!("rust.liftToFree: edits decode: {error}"));
                }
            };
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(root, "rust.liftToFree", &file_edits, false) {
            Ok(projection) => projection,
            Err(error) => return ToolResult::Error(format!("rust.liftToFree: {error}")),
        };
        record_in_ledger(ledger, "rust.liftToFree", &changes);

        let refusal_reasons = plan_value
            .get("refusal_reasons")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let mut findings = refusal_reasons
            .as_array()
            .into_iter()
            .flatten()
            .map(|reason| {
                json!({
                    "finding": "refusal_reason",
                    "method": reason["method"],
                    "reason": reason["reason"],
                })
            })
            .collect::<Vec<_>>();
        for note in plan_value["leftovers"].as_array().into_iter().flatten() {
            findings.push(json!({ "finding": "note", "detail": note }));
        }

        ToolResult::Json(json!({
            "title": plan_value["title"].as_str().unwrap_or("lift Rust methods to free functions"),
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "refusal_reasons": refusal_reasons,
            "would_change_files": would_change_files,
            "would_create_files": would_create_files,
            "provenance": "syntax_only",
        }))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn cx_in(dir: &std::path::Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    #[tokio::test]
    async fn lifts_method_and_preserves_explicit_lifetimes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub struct Helpers;\n\nimpl Helpers {\n    pub fn choose<'a>(value: &'a str) -> &'a str { value }\n}\n",
        )
        .unwrap();

        let result = RustLiftToFree(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/free.rs",
                    "itemNames": ["choose"]
                }),
                &cx_in(&root),
            )
            .await;

        let value = match result {
            ToolResult::Json(value) => value,
            ToolResult::Error(error) => panic!("unexpected error: {error}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(value["provenance"], "syntax_only");
        assert!(!value["changes"].as_array().unwrap().is_empty());
        let content = value["creates"].as_array().unwrap()[0]["content"]
            .as_str()
            .unwrap();
        assert!(content.contains("fn choose<'a>(value: &'a str) -> &'a str"));
        assert_eq!(value["refusal_reasons"], json!([]));
    }

    #[tokio::test]
    async fn mixed_methods_return_changes_and_refusal_findings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub struct Helpers { value: String }\n\nimpl Helpers {\n    pub fn upper(value: &str) -> String { value.to_uppercase() }\n\n    pub fn current(&self) -> String { self.value.clone() }\n}\n",
        )
        .unwrap();

        let result = RustLiftToFree(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/free.rs",
                    "itemNames": ["upper", "current"]
                }),
                &cx_in(&root),
            )
            .await;

        let value = match result {
            ToolResult::Json(value) => value,
            ToolResult::Error(error) => panic!("unexpected error: {error}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert!(!value["changes"].as_array().unwrap().is_empty());
        assert!(
            value["refusal_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| {
                    reason["method"] == "current"
                        && reason["reason"]
                            .as_str()
                            .is_some_and(|reason| reason.contains("self.field"))
                })
        );
        assert!(
            value["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["finding"] == "refusal_reason")
        );
    }

    #[tokio::test]
    async fn all_refused_methods_surface_planner_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub struct Helpers { value: String }\n\nimpl Helpers {\n    pub fn current(&self) -> String { self.value.clone() }\n}\n",
        )
        .unwrap();

        let result = RustLiftToFree(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/free.rs",
                    "itemNames": ["current"]
                }),
                &cx_in(&root),
            )
            .await;

        match result {
            ToolResult::Error(error) => {
                assert!(error.contains("method_lift_refused"));
                assert!(error.contains("self.field"));
            }
            other => panic!("expected planner refusal, got: {other:?}"),
        }
    }
}

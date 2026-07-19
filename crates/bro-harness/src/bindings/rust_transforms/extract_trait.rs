//! `rust.extractTrait` - extract inherent impl methods into a trait.
//!
//! Port of v1 `extract_rust_trait` from `bbox_refactor::rust_extract_trait`
//! (design section 3.1). Thin adapter over `bbox_refactor::plan`: it runs the
//! v1 analysis/synthesis verbatim, strips the MCP/plan-apply envelope, and
//! returns `{changes, creates, findings}` for the edits algebra. NEVER writes;
//! the cell feeds `changes` into `edits.merge` and `creates` into
//! `edits.createFile`, then `edits.apply`.

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

/// `rust.extractTrait` - extract selected inherent methods into a trait.
pub struct RustExtractTrait(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtractTraitInput {
    /// Source file path (workspace-relative, no `..`).
    source: String,
    /// Target trait file path (workspace-relative, no `..`).
    target: String,
    /// Inherent impl label, for example `"impl Store"`.
    impl_name: String,
    /// Name for the extracted trait.
    trait_name: String,
    /// Method names to extract.
    item_names: Vec<String>,
}

#[async_trait]
impl Tool for RustExtractTrait {
    fn name(&self) -> &str {
        "rust.extractTrait"
    }

    fn description(&self) -> &str {
        "Extract selected Rust inherent impl methods into a trait and trait impl. Reports object-safety analysis, call-site warnings, and files that require the trait in scope. NEVER writes: returns {changes, creates, findings} for edits.merge/createFile."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (workspace-relative, no `..`)." },
                "target": { "type": "string", "description": "Target trait file path (workspace-relative, no `..`). May be a new file." },
                "implName": { "type": "string", "description": "Inherent impl label, for example \"impl Store\"." },
                "traitName": { "type": "string", "description": "Name for the extracted trait." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Method names to extract from the selected impl." }
            },
            "required": ["source", "target", "implName", "traitName", "itemNames"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "extractTrait".to_string()))
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ExtractTraitInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::Error(format!("invalid input: {error}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("itemNames is required and must not be empty".to_string());
        }
        if params.impl_name.trim().is_empty() {
            return ToolResult::Error("implName is required and must not be empty".to_string());
        }
        if params.trait_name.trim().is_empty() {
            return ToolResult::Error("traitName is required and must not be empty".to_string());
        }

        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || Self::run(params, &root, &ledger)).await
    }
}

impl RustExtractTrait {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker.
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: ExtractTraitInput,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source_abs = match resolve_workspace_file(root, &params.source, "rust.extractTrait") {
            Ok(path) => path,
            Err(error) => return ToolResult::Error(format!("rust.extractTrait: {error}")),
        };
        let target_abs = match resolve_workspace_file(root, &params.target, "rust.extractTrait") {
            Ok(path) => path,
            Err(error) => return ToolResult::Error(format!("rust.extractTrait: {error}")),
        };
        let plan_params = RefactorPlanParams {
            kind: "extract_rust_trait".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: Some(target_abs.to_string_lossy().into_owned()),
            item_names: Some(params.item_names),
            item_kinds: Some(vec!["impl_method".to_string()]),
            impl_name: Some(params.impl_name),
            module_name: Some(params.trait_name),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan_params) {
            Ok(plan) => plan,
            Err(error) => {
                let message = format!("{error:#}");
                let hint = done_hint(&message);
                return ToolResult::Error(format!("rust.extractTrait: {message}{hint}"));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::Error(format!("rust.extractTrait: plan decode: {error}"));
            }
        };
        let file_edits: Vec<bbox_refactor::FileEdit> =
            match serde_json::from_value(plan_value["edits"].clone()) {
                Ok(edits) => edits,
                Err(error) => {
                    return ToolResult::Error(format!("rust.extractTrait: edits decode: {error}"));
                }
            };
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(root, "rust.extractTrait", &file_edits, false) {
            Ok(projection) => projection,
            Err(error) => return ToolResult::Error(format!("rust.extractTrait: {error}")),
        };
        record_in_ledger(ledger, "rust.extractTrait", &changes);

        let dyn_compatible = plan_value
            .get("dyn_compatible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let object_safety_report = plan_value
            .get("object_safety_report")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let call_site_warnings = plan_value
            .get("call_site_warnings")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let trait_in_scope_required = plan_value
            .get("trait_in_scope_required")
            .cloned()
            .unwrap_or_else(|| json!([]));

        let mut findings = Vec::new();
        findings.push(json!({
            "finding": "object_safety_report",
            "detail": object_safety_report,
        }));
        if call_site_warnings
            .as_array()
            .is_some_and(|values| !values.is_empty())
        {
            findings.push(json!({
                "finding": "call_site_warnings",
                "detail": call_site_warnings,
            }));
        }
        if trait_in_scope_required
            .as_array()
            .is_some_and(|values| !values.is_empty())
        {
            findings.push(json!({
                "finding": "trait_in_scope_required",
                "detail": trait_in_scope_required,
            }));
        }
        for note in plan_value["leftovers"].as_array().into_iter().flatten() {
            findings.push(json!({ "finding": "note", "detail": note }));
        }

        ToolResult::Json(json!({
            "title": plan_value["title"].as_str().unwrap_or("extract Rust trait"),
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "dyn_compatible": dyn_compatible,
            "object_safety_report": object_safety_report,
            "call_site_warnings": call_site_warnings,
            "trait_in_scope_required": trait_in_scope_required,
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
    async fn extracts_trait_and_reports_remote_trait_scope_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub struct Store;\n\nimpl Store {\n    pub fn fetch(&self, key: usize) -> usize { key }\n}\n",
        )
        .unwrap();
        std::fs::write(
            src.join("remote.rs"),
            "use crate::Store;\n\npub fn call() { Store::fetch(1); }\n",
        )
        .unwrap();

        let result = RustExtractTrait(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/store_api.rs",
                    "implName": "impl Store",
                    "traitName": "StoreApi",
                    "itemNames": ["fetch"]
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
        assert_eq!(value["dyn_compatible"], true);
        assert!(!value["changes"].as_array().unwrap().is_empty());
        let create = &value["creates"].as_array().unwrap()[0];
        assert_eq!(create["path"], "src/store_api.rs");
        assert!(
            create["content"]
                .as_str()
                .unwrap()
                .contains("pub trait StoreApi")
        );
        assert!(
            value["trait_in_scope_required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|path| path == "crate::remote")
        );
    }

    #[tokio::test]
    async fn generic_method_surfaces_object_safety_finding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub struct Store;\n\nimpl Store {\n    pub fn map<T: Clone>(&self, value: T) -> T { value }\n}\n",
        )
        .unwrap();

        let result = RustExtractTrait(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/store_api.rs",
                    "implName": "impl Store",
                    "traitName": "StoreApi",
                    "itemNames": ["map"]
                }),
                &cx_in(&root),
            )
            .await;

        let value = match result {
            ToolResult::Json(value) => value,
            ToolResult::Error(error) => panic!("unexpected error: {error}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(value["dyn_compatible"], false);
        assert!(
            value["object_safety_report"]["generic_methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|method| method == "map")
        );
        assert!(
            value["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["finding"] == "object_safety_report")
        );
    }

    #[tokio::test]
    async fn empty_item_names_are_rejected_before_planning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let result = RustExtractTrait(ledger())
            .call(
                json!({
                    "source": "src/lib.rs",
                    "target": "src/store_api.rs",
                    "implName": "impl Store",
                    "traitName": "StoreApi",
                    "itemNames": []
                }),
                &cx_in(&root),
            )
            .await;

        match result {
            ToolResult::Error(error) => assert!(error.contains("must not be empty")),
            other => panic!("expected validation error, got: {other:?}"),
        }
    }
}

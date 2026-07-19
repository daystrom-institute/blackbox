//! `rust.migrateErrorType` and `rust.migrateTypeUsages`.
//!
//! Thin, never-writing adapters over the v1 error/type migration planners.
//! Both transforms consume the public-API acknowledgement only from dispatch
//! defaults, never from a cell-authored argument.

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

const OPT_OUT_PARAM: &str = "acknowledge_public_api_change";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrateErrorTypeInput {
    source: String,
    old_text: String,
    new_text: String,
    item_names: Vec<String>,
    #[serde(default)]
    error_mapping: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MigrateTypeUsagesInput {
    source: String,
    module_name: String,
    replacement_kind: String,
    new_text: String,
}

/// `rust.migrateErrorType` - rewrite an error type in named signatures and
/// mapped construction sites.
pub struct RustMigrateErrorType(pub Arc<ProvenanceLedger>);

/// `rust.migrateTypeUsages` - replace supported uses of one Rust type.
pub struct RustMigrateTypeUsages(pub Arc<ProvenanceLedger>);

fn operator_grant(cx: &ToolCx, tool: &str) -> Option<bool> {
    cx.tool_arg_defaults
        .lookup(tool, OPT_OUT_PARAM)
        .map(|value| value.eq_ignore_ascii_case("true"))
}

fn public_api_hint(tool: &str) -> String {
    format!(
        "operator authority arrives dispatch-side; set \
         `default:{tool}.{OPT_OUT_PARAM}=true` via \
         `isolate --tool-defaults '{{\"default:{tool}.{OPT_OUT_PARAM}\":\"true\"}}'` \
         to grant this opt-out"
    )
}

fn operator_opt_outs(plan_value: &Value) -> Vec<String> {
    plan_value
        .get("operator_opt_outs_used")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn file_edits(plan_value: &Value, tool: &str) -> Result<Vec<bbox_refactor::FileEdit>, ToolResult> {
    serde_json::from_value(
        plan_value
            .get("edits")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![])),
    )
    .map_err(|error| ToolResult::Error(format!("{tool}: edits decode: {error}")))
}

fn project_plan(
    root: &std::path::Path,
    ledger: &ProvenanceLedger,
    tool: &'static str,
    plan_json: String,
    additional_findings: Vec<Value>,
) -> ToolResult {
    let plan_value: Value = match serde_json::from_str(&plan_json) {
        Ok(value) => value,
        Err(error) => return ToolResult::Error(format!("{tool}: plan decode: {error}")),
    };
    let edits = match file_edits(&plan_value, tool) {
        Ok(edits) => edits,
        Err(error) => return error,
    };
    let PlanProjection {
        changes,
        creates,
        would_change_files,
        would_create_files,
    } = match plan_to_changes_creates(root, tool, &edits, false) {
        Ok(projection) => projection,
        Err(error) => return ToolResult::Error(format!("{tool}: {error}")),
    };
    record_in_ledger(ledger, tool, &changes);

    let mut findings = additional_findings;
    for leftover in plan_value
        .get("leftovers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        findings.push(json!({ "finding": "note", "detail": leftover }));
    }

    ToolResult::Json(json!({
        "title": plan_value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(tool),
        "changes": changes,
        "creates": creates,
        "findings": findings,
        "would_change_files": would_change_files,
        "would_create_files": would_create_files,
        "operator_opt_outs_used": operator_opt_outs(&plan_value),
        "provenance": "syntax_only",
    }))
}

#[async_trait]
impl Tool for RustMigrateErrorType {
    fn name(&self) -> &str {
        "rust.migrateErrorType"
    }

    fn description(&self) -> &str {
        "Rewrite an error type in named Rust function signatures and mapped construction sites. NEVER writes: returns {changes, creates, findings, operator_opt_outs_used} for edits.merge/createFile. The public API acknowledgement arrives only through dispatch-side ToolArgDefaults."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source Rust file, workspace-relative and without `..`." },
                "oldText": { "type": "string", "description": "Existing error type name." },
                "newText": { "type": "string", "description": "Replacement error type name." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Named functions whose return signatures should change." },
                "errorMapping": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Old error variant to new error variant mapping for construction sites." }
            },
            "required": ["source", "oldText", "newText", "itemNames"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "migrateErrorType".to_string()))
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: MigrateErrorTypeInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::Error(format!("invalid input: {error}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("itemNames is required and must not be empty".to_string());
        }
        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        let grant = operator_grant(cx, "rust.migrateErrorType");
        bro_tools::tool::call_blocking(move || Self::run(params, grant, &root, &ledger)).await
    }
}

impl RustMigrateErrorType {
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: MigrateErrorTypeInput,
        grant: Option<bool>,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source = match resolve_workspace_file(root, &params.source, "rust.migrateErrorType") {
            Ok(source) => source,
            Err(error) => return ToolResult::Error(format!("rust.migrateErrorType: {error}")),
        };
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "error_mapping".to_string(),
            serde_json::to_value(params.error_mapping).expect("map serializes"),
        );
        if grant == Some(true) {
            entries.insert(OPT_OUT_PARAM.to_string(), Value::Bool(true));
        }
        let plan = RefactorPlanParams {
            kind: "rewrite_rust_error_type".to_string(),
            source: source.to_string_lossy().into_owned(),
            item_names: Some(params.item_names),
            old_text: Some(params.old_text),
            new_text: Some(params.new_text),
            toml_entries: Some(entries),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan) {
            Ok(plan) => plan,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("public_api_change_unacknowledged") {
                    return ToolResult::Error(format!(
                        "rust.migrateErrorType: {message} - {}",
                        public_api_hint("rust.migrateErrorType")
                    ));
                }
                return ToolResult::Error(format!(
                    "rust.migrateErrorType: {message}{}",
                    done_hint(&message)
                ));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::Error(format!("rust.migrateErrorType: plan decode: {error}"));
            }
        };
        let question_mark_sites = plan_value
            .get("question_mark_sites")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        project_plan(
            root,
            ledger,
            "rust.migrateErrorType",
            plan_json,
            vec![json!({
                "finding": "question_mark_sites",
                "detail": question_mark_sites,
            })],
        )
    }
}

#[async_trait]
impl Tool for RustMigrateTypeUsages {
    fn name(&self) -> &str {
        "rust.migrateTypeUsages"
    }

    fn description(&self) -> &str {
        "Migrate supported Rust type usage positions to a new concrete or trait-based replacement. NEVER writes: returns {changes, creates, findings, operator_opt_outs_used} for edits.merge/createFile. The public API acknowledgement arrives only through dispatch-side ToolArgDefaults."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source Rust file, workspace-relative and without `..`." },
                "moduleName": { "type": "string", "description": "Type name to migrate." },
                "replacementKind": { "type": "string", "enum": ["bareConcrete", "boxDyn", "arcDyn", "rcDyn", "implTrait", "genericParamTBoundedTrait"], "description": "Replacement form." },
                "newText": { "type": "string", "description": "Replacement concrete type or trait text." }
            },
            "required": ["source", "moduleName", "replacementKind", "newText"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "migrateTypeUsages".to_string()))
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: MigrateTypeUsagesInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(error) => return ToolResult::Error(format!("invalid input: {error}")),
        };
        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        let grant = operator_grant(cx, "rust.migrateTypeUsages");
        bro_tools::tool::call_blocking(move || Self::run(params, grant, &root, &ledger)).await
    }
}

impl RustMigrateTypeUsages {
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: MigrateTypeUsagesInput,
        grant: Option<bool>,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        if grant != Some(true) {
            return ToolResult::Error(format!(
                "rust.migrateTypeUsages: error.bad_input(code=public_api_change_unacknowledged): \
                 migrating type usages can change the public API - {}",
                public_api_hint("rust.migrateTypeUsages")
            ));
        }
        let source = match resolve_workspace_file(root, &params.source, "rust.migrateTypeUsages") {
            Ok(source) => source,
            Err(error) => return ToolResult::Error(format!("rust.migrateTypeUsages: {error}")),
        };
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(OPT_OUT_PARAM.to_string(), Value::Bool(true));
        let plan = RefactorPlanParams {
            kind: "migrate_rust_type_usages".to_string(),
            source: source.to_string_lossy().into_owned(),
            module_name: Some(params.module_name),
            old_text: Some(params.replacement_kind),
            new_text: Some(params.new_text),
            toml_entries: Some(entries),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan) {
            Ok(plan) => plan,
            Err(error) => {
                let message = format!("{error:#}");
                return ToolResult::Error(format!(
                    "rust.migrateTypeUsages: {message}{}",
                    done_hint(&message)
                ));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::Error(format!("rust.migrateTypeUsages: plan decode: {error}"));
            }
        };
        let migration_skipped = plan_value
            .get("migration_skipped")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        project_plan(
            root,
            ledger,
            "rust.migrateTypeUsages",
            plan_json,
            vec![json!({
                "finding": "migration_skipped",
                "detail": migration_skipped,
            })],
        )
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn cx_in(root: &std::path::Path) -> ToolCx {
        ToolCx {
            root: root.to_path_buf(),
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

    fn cx_with_grant(root: &std::path::Path, tool: &str) -> ToolCx {
        let mut defaults = BTreeMap::new();
        defaults.insert(
            format!("default:{tool}.{OPT_OUT_PARAM}"),
            "true".to_string(),
        );
        ToolCx {
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::parse_map(defaults).unwrap()),
            ..cx_in(root)
        }
    }

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    #[tokio::test]
    async fn public_error_type_refuses_without_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "pub enum OldError { Bad }\npub enum NewError { Worse }\npub fn run() -> Result<(), OldError> { Err(OldError::Bad) }\n",
        )
        .unwrap();

        let result = RustMigrateErrorType(ledger())
            .call(
                json!({
                    "source": "lib.rs",
                    "oldText": "OldError",
                    "newText": "NewError",
                    "itemNames": ["run"],
                    "errorMapping": {"Bad": "Worse"}
                }),
                &cx_in(&root),
            )
            .await;

        match result {
            ToolResult::Error(error) => {
                assert!(error.contains("public_api_change_unacknowledged"));
                assert!(error.contains("isolate --tool-defaults"));
            }
            other => panic!("expected public API refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn public_error_type_plan_audits_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "pub enum OldError { Bad }\npub enum NewError { Worse }\npub fn run() -> Result<(), OldError> { Err(OldError::Bad) }\n",
        )
        .unwrap();

        let result = RustMigrateErrorType(ledger())
            .call(
                json!({
                    "source": "lib.rs",
                    "oldText": "OldError",
                    "newText": "NewError",
                    "itemNames": ["run"],
                    "errorMapping": {"Bad": "Worse"}
                }),
                &cx_with_grant(&root, "rust.migrateErrorType"),
            )
            .await;

        let value = match result {
            ToolResult::Json(value) => value,
            ToolResult::Error(error) => panic!("expected plan with grant: {error}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert!(
            value["operator_opt_outs_used"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == OPT_OUT_PARAM)
        );
    }

    #[tokio::test]
    async fn type_migration_refuses_without_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn run(arg: Old) -> Old { arg }\n").unwrap();

        let result = RustMigrateTypeUsages(ledger())
            .call(
                json!({
                    "source": "lib.rs",
                    "moduleName": "Old",
                    "replacementKind": "bareConcrete",
                    "newText": "New"
                }),
                &cx_in(&root),
            )
            .await;

        match result {
            ToolResult::Error(error) => {
                assert!(error.contains("public_api_change_unacknowledged"));
                assert!(error.contains("isolate --tool-defaults"));
            }
            other => panic!("expected public API refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn type_migration_plan_audits_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn run(arg: Old) -> Old { arg }\n").unwrap();

        let result = RustMigrateTypeUsages(ledger())
            .call(
                json!({
                    "source": "lib.rs",
                    "moduleName": "Old",
                    "replacementKind": "bareConcrete",
                    "newText": "New"
                }),
                &cx_with_grant(&root, "rust.migrateTypeUsages"),
            )
            .await;

        let value = match result {
            ToolResult::Json(value) => value,
            ToolResult::Error(error) => panic!("expected plan with grant: {error}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert!(
            value["operator_opt_outs_used"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == OPT_OUT_PARAM)
        );
    }
}

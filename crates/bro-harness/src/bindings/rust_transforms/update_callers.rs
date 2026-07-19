//! `rust.updateCallers` - rewrite callers through a delegate field (RX-S2b).
//!
//! Port of v1 `update_rust_callers` from `bbox_refactor::rust_update_callers`
//! (design section 3.1). Companion caller-rewrite that runs after
//! `rust.moveStructFields`: for each named moved field/method, conservatively
//! rewrites `self.field` and `self.method(args)` accesses in the source impl
//! to go through a delegate field (`self.delegate.field`). Only Copy-whitelisted
//! rvalue reads and unambiguous method calls are rewritten; everything else
//! surfaces as `unrewriteable_accessors` in findings.
//!
//! Thin adapter over `bbox_refactor::plan` kind `update_rust_callers`: it runs
//! the v1 analysis/synthesis verbatim, strips the MCP/plan-apply envelope, and
//! returns `{changes, findings, counts}` for `edits.merge`. NEVER writes; the
//! cell feeds `changes` into `edits.merge`, then `edits.apply`. Same shape as
//! `rust.rewriteModuleCallers` (the other companion caller-rewrite).

use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::RefactorPlanParams;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers::{plan_to_changes_creates, record_in_ledger, resolve_workspace_file};
use crate::bindings::ledger::ProvenanceLedger;

/// `rust.updateCallers` - rewrite callers through a delegate field.
pub struct RustUpdateCallers(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCallersInput {
    /// Source file path (workspace-relative, no `..`).
    source: String,
    /// Name of the struct whose fields/methods were moved. Used to resolve
    /// Copy-whitelisted field types from the source struct.
    #[serde(default)]
    struct_name: Option<String>,
    /// The delegate field name in the source struct (e.g. `"state"`), so
    /// `self.field` becomes `self.state.field`.
    delegate_field: String,
    /// Optional target file path (workspace-relative) where the delegate type
    /// lives, used to resolve Copy-whitelisted field types when the field no
    /// longer exists in the source struct.
    #[serde(default)]
    target: Option<String>,
    /// Optional delegate type name for field-type resolution in the target.
    #[serde(default)]
    delegate_type: Option<String>,
    /// Names of the moved fields/methods whose accessors should be rewritten.
    item_names: Vec<String>,
}

#[async_trait]
impl Tool for RustUpdateCallers {
    fn name(&self) -> &str {
        "rust.updateCallers"
    }
    fn description(&self) -> &str {
        "Rewrite callers through a delegate field (RX-S2b). Companion to rust.moveStructFields: conservatively rewrites self.field and self.method(args) accesses in the source impl to go through a delegate field. Only Copy-whitelisted rvalue reads and unambiguous method calls are rewritten; the rest surface as unrewriteable_accessors in findings. NEVER writes: returns {changes, findings, counts} for edits.merge."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (workspace-relative, no `..`)." },
                "structName": { "type": "string", "description": "Name of the source struct (for Copy-whitelist field-type resolution). Optional but recommended." },
                "delegateField": { "type": "string", "description": "The delegate field name in the source struct (e.g. \"state\"), so self.field becomes self.state.field." },
                "target": { "type": "string", "description": "Optional target file path where the delegate type lives (for field-type resolution when the field moved out of the source struct)." },
                "delegateType": { "type": "string", "description": "Optional delegate type name for field-type resolution in the target." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Names of the moved fields/methods whose accessors should be rewritten." }
            },
            "required": ["source", "delegateField", "itemNames"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "updateCallers".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: UpdateCallersInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid input: {e}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("itemNames is required and must not be empty".to_string());
        }
        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || Self::run(params, &root, &ledger)).await
    }
}

impl RustUpdateCallers {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker
    // (concurrency-model section 5).
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: UpdateCallersInput,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source_abs = match resolve_workspace_file(root, &params.source, "rust.updateCallers") {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.updateCallers: {e}")),
        };
        let target_abs = params
            .target
            .as_deref()
            .map(|t| resolve_workspace_file(root, t, "rust.updateCallers"))
            .transpose();
        let target_path = match target_abs {
            Ok(opt) => opt.map(|p| p.to_string_lossy().into_owned()),
            Err(e) => return ToolResult::Error(format!("rust.updateCallers: {e}")),
        };
        let plan_params = RefactorPlanParams {
            kind: "update_rust_callers".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: target_path,
            item_names: Some(params.item_names.clone()),
            impl_name: params.struct_name.clone(),
            delegate_field: Some(params.delegate_field.clone()),
            delegate_type: params.delegate_type.clone(),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan_params) {
            Ok(json) => json,
            Err(e) => {
                let msg = format!("{e:#}");
                let hint = super::helpers::done_hint(&msg);
                return ToolResult::Error(format!("rust.updateCallers: {msg}{hint}"));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(v) => v,
            Err(e) => return ToolResult::Error(format!("rust.updateCallers: plan decode: {e}")),
        };
        let file_edits_json = plan_value
            .get("edits")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        let file_edits: Vec<bbox_refactor::FileEdit> = match serde_json::from_value(file_edits_json)
        {
            Ok(edits) => edits,
            Err(e) => return ToolResult::Error(format!("rust.updateCallers: edits decode: {e}")),
        };
        let projection =
            match plan_to_changes_creates(root, "rust.updateCallers", &file_edits, false) {
                Ok(proj) => proj,
                Err(e) => return ToolResult::Error(format!("rust.updateCallers: {e}")),
            };
        // update_rust_callers never creates files (it edits the source in
        // place), so creates is always empty; but we keep the projection for
        // shape consistency.
        record_in_ledger(ledger, "rust.updateCallers", &projection.changes);
        let mut findings: Vec<Value> = Vec::new();
        // Surface unrewriteable accessors (field writes, ambiguous calls).
        if let Some(unrewriteable) = plan_value
            .get("unrewriteable_accessors")
            .and_then(|v| v.as_array())
            && !unrewriteable.is_empty()
        {
            findings.push(json!({
                "finding": "unrewriteable_accessors",
                "detail": unrewriteable,
            }));
        }
        // Surface borrow promotions (field-read sites that may need &mut self
        // on the delegate).
        if let Some(promotions) = plan_value
            .get("borrow_promotions")
            .and_then(|v| v.as_array())
            && !promotions.is_empty()
        {
            findings.push(json!({
                "finding": "borrow_promotions",
                "detail": promotions,
            }));
        }
        // Surface overlapping rewrite sites excluded from FileEdits.
        if let Some(overlapping) = plan_value
            .get("overlapping_rewrite_sites")
            .and_then(|v| v.as_array())
            && !overlapping.is_empty()
        {
            findings.push(json!({
                "finding": "overlapping_rewrite_sites",
                "detail": overlapping,
            }));
        }
        let rewrites = projection.changes.len() as u64;
        let files_touched = if rewrites > 0 { 1u64 } else { 0u64 };
        ToolResult::Json(json!({
            "changes": projection.changes,
            "findings": findings,
            "counts": {
                "files_touched": files_touched,
                "rewrites": rewrites,
            },
            "provenance": "syntax_only",
        }))
    }
}

// Test scopes are a sanctioned context for blocking I/O (clippy.toml I2):
// tempdir fixtures run on nextest's per-test process, never on a tokio worker.
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

    /// A Copy rvalue field read (`self.count` where `count: u32`) rewrites to
    /// `self.state.count()` through the delegate field.
    #[tokio::test]
    async fn copy_rvalue_read_rewrites_through_delegate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("server.rs"),
            "struct BigServer {\n    count: u32,\n    state: ServerState,\n}\n\n\
             impl BigServer {\n    fn get_count(&self) -> u32 {\n        self.count\n    }\n}\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustUpdateCallers(ledger());
        let result = tool
            .call(
                json!({
                    "source": "src/server.rs",
                    "structName": "BigServer",
                    "delegateField": "state",
                    "itemNames": ["count"]
                }),
                &cx,
            )
            .await;

        let val = match result {
            ToolResult::Json(v) => v,
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(val["provenance"], "syntax_only");
        let changes = val["changes"].as_array().expect("changes array");
        assert!(
            !changes.is_empty(),
            "Copy rvalue read should produce at least one rewrite"
        );
        let replacement = changes
            .iter()
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            replacement.contains("self.state.count"),
            "expected rewrite through delegate: {replacement}"
        );
        let counts = &val["counts"];
        assert_eq!(counts["rewrites"], json!(changes.len() as u64));
        assert_eq!(counts["files_touched"], json!(1u64));
    }

    /// A field write (`self.count = 5`) goes to `unrewriteable_accessors` in
    /// findings, NOT into changes.
    #[tokio::test]
    async fn field_write_goes_to_unrewriteable_accessors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("server.rs"),
            "struct BigServer {\n    count: u32,\n    state: ServerState,\n}\n\n\
             impl BigServer {\n    fn set_count(&mut self) {\n        self.count = 5;\n    }\n}\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustUpdateCallers(ledger());
        let result = tool
            .call(
                json!({
                    "source": "src/server.rs",
                    "structName": "BigServer",
                    "delegateField": "state",
                    "itemNames": ["count"]
                }),
                &cx,
            )
            .await;

        let val = match result {
            ToolResult::Json(v) => v,
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        };
        // No rewrites: the only access is a write.
        assert_eq!(val["counts"]["rewrites"], json!(0u64));
        let findings = val["findings"].as_array().expect("findings array");
        let has_unrewriteable = findings
            .iter()
            .any(|f| f["finding"] == "unrewriteable_accessors");
        assert!(
            has_unrewriteable,
            "expected unrewriteable_accessors finding for a field write: {val}"
        );
    }
}

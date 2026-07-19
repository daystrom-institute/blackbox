//! `rust.moveStructFields` - move named fields between structs (RX-S1).
//!
//! Port of v1 `move_rust_struct_fields` from `bbox_refactor::rust_move_fields`
//! (design section 3.1, section 2.4). Thin adapter over `bbox_refactor::plan`:
//! it runs the v1 analysis/synthesis verbatim, strips the MCP/plan-apply
//! envelope, and returns `{changes, creates, findings}` for the edits algebra.
//! NEVER writes; the cell feeds `changes` into `edits.merge` and `creates`
//! into `edits.createFile`, then `edits.apply`.
//!
//! RX-V1 channel (design section 2.4 + section 8.2): `acknowledge_repr` is an
//! operator-authority flag, NOT a cell-authored input. The binding declares no
//! `acknowledge_repr` schema param (a cell passing one gets a schema error).
//! Instead the binding queries `cx.tool_arg_defaults` host-side for a grant on
//! `("rust.moveStructFields", "acknowledge_repr")`. When the grant is present
//! and truthy, the binding injects `acknowledge_repr=true` into the planner's
//! `toml_entries` and surfaces `operator_opt_outs_used` in the result. When the
//! grant is absent and the source struct carries a non-default `#[repr(...)]`,
//! the planner refuses with `repr_unacknowledged`; the binding surfaces that
//! refusal with a hint pointing at the dispatch-side default the operator must
//! supply.

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

/// `rust.moveStructFields` - move named fields from one struct to another.
pub struct RustMoveStructFields(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveStructFieldsInput {
    /// Source file path (workspace-relative, no `..`).
    source: String,
    /// Target file path (workspace-relative, no `..`). May equal `source`
    /// when the target struct lives in the same file.
    target: String,
    /// Name of the source struct whose fields are being moved.
    struct_name: String,
    /// Field names to move (declaration order is preserved).
    item_names: Vec<String>,
    /// Optional visibility override applied to moved fields in the target
    /// (e.g. `"pub"`, `"pub(crate)"`). Defaults to preserving source visibility.
    #[serde(default)]
    visibility: Option<String>,
}

/// The RX-V1 opt-out param this binding consumes from the dispatch-side
/// `ToolArgDefaults` table (never from cell input).
const OPT_OUT_PARAM: &str = "acknowledge_repr";

#[async_trait]
impl Tool for RustMoveStructFields {
    fn name(&self) -> &str {
        "rust.moveStructFields"
    }
    fn description(&self) -> &str {
        "Move named fields from one struct to another (RX-S1). Thin adapter over the v1 move_rust_struct_fields planner. NEVER writes: returns {changes, creates, findings, operator_opt_outs_used} for edits.merge/createFile. The acknowledge_repr operator opt-out (required when the source struct has a non-default #[repr]) arrives dispatch-side via ToolArgDefaults, never as cell input."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (workspace-relative, no `..`)." },
                "target": { "type": "string", "description": "Target file path (workspace-relative, no `..`). May equal source when the target struct is in the same file." },
                "structName": { "type": "string", "description": "Name of the source struct whose fields are being moved." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Field names to move (declaration order preserved)." },
                "visibility": { "type": "string", "description": "Optional visibility override applied to moved fields in the target (e.g. \"pub\", \"pub(crate)\"). Defaults to preserving source visibility." }
            },
            "required": ["source", "target", "structName", "itemNames"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "moveStructFields".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: MoveStructFieldsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid input: {e}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("itemNames is required and must not be empty".to_string());
        }
        // RX-V1 (design section 2.4): the acknowledge_repr grant arrives
        // dispatch-side via ToolArgDefaults, never as cell input. The binding
        // queries the table host-side and treats a truthy value as the operator
        // opt-out. A cell passing acknowledge_repr in the JSON input hits the
        // schema (no such param declared above), so cell authorship is a schema
        // error, not confirm theater.
        let acknowledge_repr_grant = cx
            .tool_arg_defaults
            .lookup("rust.moveStructFields", OPT_OUT_PARAM)
            .map(|v| v.eq_ignore_ascii_case("true"));
        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || {
            Self::run(params, acknowledge_repr_grant, &root, &ledger)
        })
        .await
    }
}

impl RustMoveStructFields {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker
    // (concurrency-model section 5).
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: MoveStructFieldsInput,
        acknowledge_repr_grant: Option<bool>,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source_abs = match resolve_workspace_file(root, &params.source, "rust.moveStructFields")
        {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.moveStructFields: {e}")),
        };
        let target_abs = match resolve_workspace_file(root, &params.target, "rust.moveStructFields")
        {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.moveStructFields: {e}")),
        };
        let mut entries = std::collections::BTreeMap::new();
        if let Some(true) = acknowledge_repr_grant {
            entries.insert(OPT_OUT_PARAM.to_string(), Value::Bool(true));
        }
        let toml_entries = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
        let plan_params = RefactorPlanParams {
            kind: "move_rust_struct_fields".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: Some(target_abs.to_string_lossy().into_owned()),
            item_names: Some(params.item_names.clone()),
            impl_name: Some(params.struct_name.clone()),
            visibility: params.visibility.clone(),
            toml_entries,
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan_params) {
            Ok(json) => json,
            Err(e) => {
                let msg = format!("{e:#}");
                // RX-V1 refusal: the planner bailed with repr_unacknowledged
                // because no operator grant was present. Surface the dispatch
                // channel the operator must use (design section 2.4).
                if msg.contains("repr_unacknowledged") {
                    return ToolResult::Error(format!(
                        "rust.moveStructFields: {msg} - operator authority arrives dispatch-side; \
                         set default:rust.moveStructFields.{OPT_OUT_PARAM}=true in dispatch config \
                         (ToolArgDefaults) to grant this opt-out"
                    ));
                }
                let hint = done_hint(&msg);
                return ToolResult::Error(format!("rust.moveStructFields: {msg}{hint}"));
            }
        };
        let plan_value: Value = match serde_json::from_str(&plan_json) {
            Ok(v) => v,
            Err(e) => return ToolResult::Error(format!("rust.moveStructFields: plan decode: {e}")),
        };
        // Extract the FileEdit array from the flattened RefactorPlan.
        let file_edits_json = plan_value
            .get("edits")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        let file_edits: Vec<bbox_refactor::FileEdit> = match serde_json::from_value(file_edits_json)
        {
            Ok(edits) => edits,
            Err(e) => {
                return ToolResult::Error(format!("rust.moveStructFields: edits decode: {e}"));
            }
        };
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(root, "rust.moveStructFields", &file_edits, false) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.moveStructFields: {e}")),
        };
        record_in_ledger(ledger, "rust.moveStructFields", &changes);
        // Surface the v1 audit field (operator_opt_outs_used) so the applied
        // EditSet lineage carries the consumed opt-out.
        let operator_opt_outs_used: Vec<String> = plan_value
            .get("operator_opt_outs_used")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut findings: Vec<Value> = Vec::new();
        for note in plan_value
            .get("leftovers")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        // Report remaining source accessors when present (deep analysis).
        if let Some(accessors) = plan_value
            .get("remaining_source_accessors")
            .and_then(|v| v.as_array())
            && !accessors.is_empty()
        {
            findings.push(json!({
                "finding": "remaining_source_accessors",
                "detail": accessors,
            }));
        }
        // Report inherited generics when present.
        if let Some(generics) = plan_value
            .get("inherited_generics")
            .and_then(|v| v.as_array())
            && !generics.is_empty()
        {
            findings.push(json!({
                "finding": "inherited_generics",
                "detail": generics,
            }));
        }
        let title = plan_value
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("move struct fields")
            .to_string();
        ToolResult::Json(json!({
            "title": title,
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "would_change_files": would_change_files,
            "would_create_files": would_create_files,
            "operator_opt_outs_used": operator_opt_outs_used,
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

    fn cx_with_grant(dir: &std::path::Path) -> ToolCx {
        let mut map = BTreeMap::new();
        map.insert(
            "default:rust.moveStructFields.acknowledge_repr".to_string(),
            "true".to_string(),
        );
        let defaults = bro_tools::ToolArgDefaults::parse_map(map).unwrap();
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(defaults),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    /// A clean field move (no repr, no remaining accessors) produces changes
    /// that relocate the field to the target struct.
    #[tokio::test]
    async fn clean_move_produces_relocating_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("server.rs"),
            "struct BigServer {\n    count: u32,\n    state: ServerState,\n}\n",
        )
        .unwrap();
        std::fs::write(src.join("state.rs"), "struct ServerState {\n}\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustMoveStructFields(ledger());
        let result = tool
            .call(
                json!({
                    "source": "src/server.rs",
                    "target": "src/state.rs",
                    "structName": "BigServer",
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
            "clean move should propose at least one change"
        );
        // The target struct should receive the moved field in the creates or
        // changes payload. Check the combined replacement text.
        let all_text = changes
            .iter()
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let creates_text = val["creates"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| c["content"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let combined = format!("{all_text}\n{creates_text}");
        assert!(
            combined.contains("count: u32"),
            "moved field should appear in target edits: {combined}"
        );
        // operator_opt_outs_used is empty for a clean (non-repr) move.
        assert_eq!(val["operator_opt_outs_used"], json!([]));
    }

    /// A repr-tagged struct WITHOUT the operator grant refuses with the
    /// RX-V1 refusal naming the dispatch channel.
    #[tokio::test]
    async fn repr_struct_refuses_without_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("packed.rs"),
            "#[repr(C)]\nstruct Packed {\n    x: u32,\n    y: u32,\n}\n",
        )
        .unwrap();
        std::fs::write(src.join("state.rs"), "struct Target {\n}\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustMoveStructFields(ledger());
        let result = tool
            .call(
                json!({
                    "source": "src/packed.rs",
                    "target": "src/state.rs",
                    "structName": "Packed",
                    "itemNames": ["x"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(
                    e.contains("repr_unacknowledged"),
                    "expected the RX-V1 repr refusal, got: {e}"
                );
                assert!(
                    e.contains("dispatch-side"),
                    "refusal should name the dispatch channel: {e}"
                );
                assert!(
                    e.contains("acknowledge_repr=true"),
                    "refusal should name the exact default the operator must set: {e}"
                );
            }
            other => panic!("expected RX-V1 refusal error, got: {other:?}"),
        }
    }

    /// A repr-tagged struct WITH the operator grant proceeds and reports
    /// `acknowledge_repr` in `operator_opt_outs_used`.
    #[tokio::test]
    async fn repr_struct_proceeds_with_operator_grant() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("packed.rs"),
            "#[repr(C)]\nstruct Packed {\n    x: u32,\n    y: u32,\n}\n",
        )
        .unwrap();
        std::fs::write(src.join("state.rs"), "struct Target {\n}\n").unwrap();

        let cx = cx_with_grant(&root);
        let tool = RustMoveStructFields(ledger());
        let result = tool
            .call(
                json!({
                    "source": "src/packed.rs",
                    "target": "src/state.rs",
                    "structName": "Packed",
                    "itemNames": ["x"]
                }),
                &cx,
            )
            .await;

        let val = match result {
            ToolResult::Json(v) => v,
            ToolResult::Error(e) => panic!("expected success with grant, got error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        };
        let opt_outs = val["operator_opt_outs_used"].as_array().expect("array");
        assert!(
            opt_outs.iter().any(|o| o == "acknowledge_repr"),
            "operator_opt_outs_used should report acknowledge_repr: {val}"
        );
    }
}

//! `rust.moduleWiring` and `rust.setVisibility` - the wiring/visibility bindings.
//!
//! Ports of the v1 `rust_module_wiring` (+ absorbed mod/use micro-kinds) and
//! `rewrite_rust_item_visibility` + `rewrite_rust_field_visibility`. Each is
//! a thin adapter over `bbox_refactor::plan` (design §3.1, §7): it runs the
//! v1 analysis/synthesis verbatim, strips the MCP/plan-apply envelope, and
//! returns `{changes, findings}` for the edits algebra. NEVER writes; the
//! cell feeds `changes` into `edits.merge`, then `edits.apply`.
//!
//! `rust.moduleWiring` is idempotent: add_mod/add_use reject duplicates;
//! remove_mod/remove_use reject missing targets. `rust.setVisibility`
//! preserves `async`/`unsafe`/`const` qualifiers (the planner rewrites only
//! the visibility prefix) and supports impl-method selection via `implName`.

use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::RefactorPlanParams;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers::{
    PlanProjection, plan_to_changes_creates, record_in_ledger, resolve_workspace_file,
};
use crate::bindings::ledger::ProvenanceLedger;

// ───────────────────────────── moduleWiring ─────────────────────────────

/// `rust.moduleWiring` - one conservative module-graph edit. Ports the v1
/// `rust_module_wiring` planner: add/remove `mod`, add/remove `use`,
/// idempotent, rejects duplicates/missing, tree-sitter validated.
pub struct RustModuleWiring(pub Arc<ProvenanceLedger>);

/// One wiring action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WiringAction {
    AddMod,
    RemoveMod,
    AddUse,
    RemoveUse,
}

impl WiringAction {
    fn as_str(self) -> &'static str {
        match self {
            WiringAction::AddMod => "add_mod",
            WiringAction::RemoveMod => "remove_mod",
            WiringAction::AddUse => "add_use",
            WiringAction::RemoveUse => "remove_use",
        }
    }
}

#[derive(Deserialize)]
struct ModuleWiringInput {
    /// Source file to edit, workspace-relative.
    source: String,
    /// One of: add_mod, remove_mod, add_use, remove_use.
    action: String,
    /// Module name for mod actions.
    #[serde(default, rename = "moduleName", alias = "module_name")]
    module_name: Option<String>,
    /// Use path (e.g. `std::collections::HashMap`, `child::{A, B}`) for use
    /// actions.
    #[serde(default, rename = "usePath", alias = "use_path")]
    use_path: Option<String>,
    /// Optional visibility prefix for the emitted declaration
    /// (`pub`, `pub(crate)`, `pub(super)`, or empty/private).
    #[serde(default)]
    visibility: Option<String>,
}

impl ModuleWiringInput {
    fn resolve_action(&self) -> Result<WiringAction, String> {
        match self.action.as_str() {
            "add_mod" => Ok(WiringAction::AddMod),
            "remove_mod" => Ok(WiringAction::RemoveMod),
            "add_use" => Ok(WiringAction::AddUse),
            "remove_use" => Ok(WiringAction::RemoveUse),
            other => Err(format!(
                "rust.moduleWiring: unsupported action `{other}`; supported: add_mod, remove_mod, add_use, remove_use"
            )),
        }
    }
}

#[async_trait]
impl Tool for RustModuleWiring {
    fn name(&self) -> &str {
        "rust.moduleWiring"
    }
    fn description(&self) -> &str {
        "One conservative Rust module-graph edit: add_mod, remove_mod, add_use, or remove_use. Idempotent (rejects duplicates and missing targets). Tree-sitter validated. NEVER writes: feed {changes} into edits.merge."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file to edit, workspace-relative." },
                "action": { "type": "string", "enum": ["add_mod", "remove_mod", "add_use", "remove_use"] },
                "moduleName": { "type": "string", "description": "Module name for mod actions." },
                "usePath": { "type": "string", "description": "Use path for use actions (e.g. `std::collections::HashMap`, `child::{A, B}`)." },
                "visibility": { "type": "string", "description": "Optional visibility prefix (`pub`, `pub(crate)`, `pub(super)`, or empty/private)." }
            },
            "required": ["source", "action"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "moduleWiring".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let root = cx.root.clone();
        let args: ModuleWiringInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("rust.moduleWiring: bad input: {e}")),
        };
        let action = match args.resolve_action() {
            Ok(action) => action,
            Err(e) => return ToolResult::Error(e),
        };
        let source_abs = match resolve_workspace_file(&root, &args.source, "rust.moduleWiring") {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.moduleWiring: {e}")),
        };
        // Validate action-specific required fields before calling the planner
        // so the error is actionable.
        match action {
            WiringAction::AddMod | WiringAction::RemoveMod => {
                if args
                    .module_name
                    .as_deref()
                    .map(str::is_empty)
                    .unwrap_or(true)
                {
                    return ToolResult::Error(format!(
                        "rust.moduleWiring: moduleName is required for {}",
                        action.as_str()
                    ));
                }
            }
            WiringAction::AddUse | WiringAction::RemoveUse => {
                if args.use_path.as_deref().map(str::is_empty).unwrap_or(true) {
                    return ToolResult::Error(format!(
                        "rust.moduleWiring: usePath is required for {}",
                        action.as_str()
                    ));
                }
            }
        }
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "action".to_string(),
            Value::String(action.as_str().to_string()),
        );
        let params = RefactorPlanParams {
            kind: "rust_module_wiring".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            module_name: args.module_name.clone(),
            use_path: args.use_path.clone(),
            visibility: args.visibility.clone(),
            toml_entries: Some(entries),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&params) {
            Ok(json) => json,
            Err(e) => return ToolResult::Error(format!("rust.moduleWiring: {e}")),
        };
        let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
            Ok(plan) => plan,
            Err(e) => return ToolResult::Error(format!("rust.moduleWiring: plan decode: {e}")),
        };
        let PlanProjection {
            changes,
            would_change_files,
            ..
        } = match plan_to_changes_creates(&root, "rust.moduleWiring", &plan.edits, false) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.moduleWiring: {e}")),
        };
        record_in_ledger(&self.0, "rust.moduleWiring", &changes);
        let mut findings: Vec<Value> = Vec::new();
        for note in &plan.leftovers {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        ToolResult::Json(json!({
            "title": plan.title,
            "changes": changes,
            "findings": findings,
            "action": action.as_str(),
            "would_change_files": would_change_files,
            "provenance": "syntax_only",
        }))
    }
}

// ───────────────────────────── setVisibility ────────────────────────────

/// `rust.setVisibility` - rewrite the visibility of top-level Rust items,
/// impl methods, or struct fields. Ports `rewrite_rust_item_visibility` +
/// `rewrite_rust_field_visibility` as one transform with a `target_kind`
/// selector.
pub struct RustSetVisibility(pub Arc<ProvenanceLedger>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VisibilityTarget {
    /// Top-level items (fns, structs, enums, ...). Calls
    /// `rewrite_rust_item_visibility`.
    Item,
    /// Impl methods. Calls `rewrite_rust_item_visibility` with
    /// `item_kinds=["impl_method"]` and optional `impl_name` disambiguation.
    Method,
    /// Struct fields. Calls `rewrite_rust_field_visibility`.
    Field,
}

impl Default for VisibilityTarget {
    fn default() -> Self {
        VisibilityTarget::Item
    }
}

#[derive(Deserialize)]
struct SetVisibilityInput {
    /// Source file, workspace-relative.
    source: String,
    /// New visibility: `pub`, `pub(crate)`, `pub(super)`, or `private`
    /// (empty prefix).
    visibility: String,
    /// What to rewrite: `item` (default), `method`, or `field`.
    #[serde(default, rename = "targetKind", alias = "target_kind")]
    target_kind: VisibilityTarget,
    /// Item/struct/method names. Required.
    #[serde(default, rename = "itemNames", alias = "item_names")]
    item_names: Vec<String>,
    /// Impl name disambiguator for method targets (which impl block).
    #[serde(default, rename = "implName", alias = "impl_name")]
    impl_name: Option<String>,
}

impl SetVisibilityInput {
    fn planner_kind(&self) -> &'static str {
        match self.target_kind {
            VisibilityTarget::Item | VisibilityTarget::Method => "rewrite_rust_item_visibility",
            VisibilityTarget::Field => "rewrite_rust_field_visibility",
        }
    }
}

#[async_trait]
impl Tool for RustSetVisibility {
    fn name(&self) -> &str {
        "rust.setVisibility"
    }
    fn description(&self) -> &str {
        "Rewrite visibility of top-level Rust items, impl methods, or struct fields. Preserves async/unsafe/const qualifiers (only the visibility prefix is rewritten). targetKind: item (default), method, or field. implName disambiguates methods when multiple impls define the same name. NEVER writes: feed {changes} into edits.merge."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file, workspace-relative." },
                "visibility": { "type": "string", "description": "New visibility: `pub`, `pub(crate)`, `pub(super)`, or `private` (empty prefix)." },
                "targetKind": { "type": "string", "enum": ["item", "method", "field"], "description": "What to rewrite. Defaults to `item`." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Item/struct/method names. Required." },
                "implName": { "type": "string", "description": "Impl name disambiguator for method targets." }
            },
            "required": ["source", "visibility", "itemNames"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "setVisibility".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let root = cx.root.clone();
        let args: SetVisibilityInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("rust.setVisibility: bad input: {e}")),
        };
        if args.item_names.is_empty() {
            return ToolResult::Error("rust.setVisibility: itemNames is required".to_string());
        }
        let source_abs = match resolve_workspace_file(&root, &args.source, "rust.setVisibility") {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.setVisibility: {e}")),
        };
        let item_kinds = match args.target_kind {
            VisibilityTarget::Method => Some(vec!["impl_method".to_string()]),
            _ => None,
        };
        if args.target_kind == VisibilityTarget::Method && args.impl_name.is_none() {
            // Warn (not refuse): ambiguous method names will be refused by
            // the planner with an actionable message; surface the hint now.
            // Only emit if the name is potentially ambiguous (more than one
            // impl could define it). We let the planner decide.
        }
        let params = RefactorPlanParams {
            kind: args.planner_kind().to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            item_names: Some(args.item_names.clone()),
            item_kinds,
            impl_name: args.impl_name.clone(),
            visibility: Some(args.visibility.clone()),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&params) {
            Ok(json) => json,
            Err(e) => return ToolResult::Error(format!("rust.setVisibility: {e}")),
        };
        let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
            Ok(plan) => plan,
            Err(e) => return ToolResult::Error(format!("rust.setVisibility: plan decode: {e}")),
        };
        let PlanProjection {
            changes,
            would_change_files,
            ..
        } = match plan_to_changes_creates(&root, "rust.setVisibility", &plan.edits, false) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.setVisibility: {e}")),
        };
        record_in_ledger(&self.0, "rust.setVisibility", &changes);
        let mut findings: Vec<Value> = Vec::new();
        for note in &plan.leftovers {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        for item in &plan.items {
            let mut finding = serde_json::to_value(item).unwrap_or_default();
            finding["finding"] = json!("visibility_rewritten");
            findings.push(finding);
        }
        ToolResult::Json(json!({
            "title": plan.title,
            "changes": changes,
            "findings": findings,
            "target_kind": match args.target_kind {
                VisibilityTarget::Item => "item",
                VisibilityTarget::Method => "method",
                VisibilityTarget::Field => "field",
            },
            "would_change_files": would_change_files,
            "provenance": "syntax_only",
        }))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    fn cx_in(dir: &std::path::Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    fn apply_changes(source: &str, result: &Value) -> String {
        let mut text_edits: Vec<bbox_refactor::TextEdit> = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| bbox_refactor::TextEdit {
                byte_start: change["span"]["byte_start"].as_u64().unwrap() as usize,
                byte_end: change["span"]["byte_end"].as_u64().unwrap() as usize,
                replacement: change["new_text"].as_str().unwrap().to_string(),
            })
            .collect();
        text_edits.sort_by_key(|edit| std::cmp::Reverse(edit.byte_start));
        let mut out = source.to_string();
        for edit in &text_edits {
            out.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        out
    }

    // moduleWiring add_mod: inserts `mod child;`, idempotent on re-add.
    #[tokio::test]
    async fn module_wiring_add_mod_inserts_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = "pub fn outer() {}\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustModuleWiring(ledger())
                .call(
                    json!({
                        "source": "parent.rs",
                        "action": "add_mod",
                        "moduleName": "child"
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["action"], "add_mod", "{result}");
        let after = apply_changes(src, &result);
        assert!(after.contains("mod child;"), "{after}");
    }

    // moduleWiring add_use: idempotent (refuses duplicate verbatim decl).
    #[tokio::test]
    async fn module_wiring_add_use_refuses_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = "use std::collections::HashMap;\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        match RustModuleWiring(ledger())
            .call(
                json!({
                    "source": "parent.rs",
                    "action": "add_use",
                    "usePath": "std::collections::HashMap"
                }),
                &cx,
            )
            .await
        {
            ToolResult::Error(message) => {
                assert!(
                    message.contains("already exists"),
                    "expected duplicate refusal, got: {message}"
                );
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    // moduleWiring remove_use: drops the line.
    #[tokio::test]
    async fn module_wiring_remove_use_drops_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = "use std::collections::HashMap;\n\nfn main() {}\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustModuleWiring(ledger())
                .call(
                    json!({
                        "source": "parent.rs",
                        "action": "remove_use",
                        "usePath": "std::collections::HashMap"
                    }),
                    &cx,
                )
                .await,
        );
        let after = apply_changes(src, &result);
        assert!(!after.contains("HashMap"), "{after}");
    }

    // moduleWiring rejects an unsupported action.
    #[tokio::test]
    async fn module_wiring_rejects_unknown_action() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("parent.rs"), "fn main() {}\n").unwrap();
        let cx = cx_in(&root);
        match RustModuleWiring(ledger())
            .call(json!({ "source": "parent.rs", "action": "bogus" }), &cx)
            .await
        {
            ToolResult::Error(message) => {
                let message = message.to_string();
                assert!(
                    message.contains("unsupported action") || message.contains("bad input"),
                    "expected action refusal, got: {message}"
                );
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    // setVisibility item: rewrites a private fn to pub(crate).
    #[tokio::test]
    async fn set_visibility_item_bumps_fn_to_pub_crate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // The qualifier `async` must survive the rewrite.
        let src = "async fn helper() {}\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustSetVisibility(ledger())
                .call(
                    json!({
                        "source": "parent.rs",
                        "visibility": "pub(crate)",
                        "itemNames": ["helper"]
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["target_kind"], "item", "{result}");
        let after = apply_changes(src, &result);
        assert!(
            after.contains("pub(crate) async fn helper"),
            "qualifier must survive: {after}"
        );
    }

    // setVisibility field: bumps struct fields.
    #[tokio::test]
    async fn set_visibility_field_bumps_struct_fields() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = "struct Data {\n    name: String,\n    value: u32,\n}\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustSetVisibility(ledger())
                .call(
                    json!({
                        "source": "parent.rs",
                        "visibility": "pub",
                        "targetKind": "field",
                        "itemNames": ["Data"]
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["target_kind"], "field", "{result}");
        let after = apply_changes(src, &result);
        assert!(after.contains("pub name: String"), "{after}");
        assert!(after.contains("pub value: u32"), "{after}");
    }

    // setVisibility method: rewrites an impl method, implName disambiguates.
    #[tokio::test]
    async fn set_visibility_method_with_impl_name_disambiguation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = "struct A;\nstruct B;\n\nimpl A {\n    fn go(&self) {}\n}\n\nimpl B {\n    fn go(&self) {}\n}\n";
        std::fs::write(root.join("parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustSetVisibility(ledger())
                .call(
                    json!({
                        "source": "parent.rs",
                        "visibility": "pub",
                        "targetKind": "method",
                        "itemNames": ["go"],
                        "implName": "impl B"
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["target_kind"], "method", "{result}");
        let after = apply_changes(src, &result);
        // Only impl B's go is bumped; impl A's stays private.
        assert!(after.contains("impl B {\n    pub fn go"), "{after}");
        assert!(after.contains("impl A {\n    fn go"), "{after}");
    }
}

//! `rust.organizeImports` - minimize Rust wildcard imports.
//!
//! Port of v1 `rust_minimize_imports` from `bbox_refactor::rust` (design
//! section 3.1). The minimize mode rewrites wildcard `use` declarations
//! (`use foo::*;`) whose source module resolves to a local Rust file and
//! whose imported names are directly referenced, expanding them into an
//! explicit `use foo::{A, B};`. It is a thin adapter over
//! `bbox_refactor::plan` kind `rust_minimize_imports`: it runs the v1
//! analysis/synthesis verbatim, strips the MCP/plan-apply envelope, and
//! returns `{changes, creates, findings}` for the edits algebra. NEVER
//! writes; the cell feeds `changes` into `edits.merge`, then `edits.apply`.
//!
//! `mode="organize"` is the future rust-analyzer `source.organizeImports`
//! path (`lsp_verified`). It lands with `lsp.assist` (phase 2); this binding
//! refuses it with a structured error rather than stubbing a fake organize,
//! so callers never silently get a different operation than they asked for.

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

/// `rust.organizeImports` - minimize Rust wildcard imports (phase 1.5).
pub struct RustOrganizeImports(pub Arc<ProvenanceLedger>);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum OrganizeMode {
    /// Syntactic wildcard-minimizer (v1 `rust_minimize_imports`,
    /// `indexed_hints`). Rewrites resolvable wildcard `use` decls into
    /// explicit `use path::{A, B};` for directly-referenced names.
    #[default]
    Minimize,
    /// rust-analyzer `source.organizeImports` (`lsp_verified`). Lands with
    /// `lsp.assist` (phase 2); refused today.
    Organize,
}

impl OrganizeMode {
    fn as_str(self) -> &'static str {
        match self {
            OrganizeMode::Minimize => "minimize",
            OrganizeMode::Organize => "organize",
        }
    }

    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("") => Ok(OrganizeMode::default()),
            Some("minimize") => Ok(OrganizeMode::Minimize),
            Some("organize") => Ok(OrganizeMode::Organize),
            Some(other) => Err(format!(
                "rust.organizeImports: mode must be \"minimize\" or \"organize\", got `{other}`"
            )),
        }
    }
}

#[derive(Deserialize)]
struct OrganizeImportsInput {
    /// Source file path (workspace-relative, no `..`).
    source: String,
    /// `"minimize"` (default) or `"organize"`.
    #[serde(default)]
    mode: Option<String>,
    /// Optional allowlist of wildcard base paths to preserve verbatim
    /// (`["std::io", "crate::prelude"]`). Pass-through to the planner's
    /// `allow_wildcards` toml entry.
    #[serde(default)]
    allow_wildcards: Option<Vec<String>>,
    /// When true, wildcard imports with no directly-referenced names are
    /// deleted instead of left as leftovers. Pass-through to the planner's
    /// `remove_unused_wildcards` toml entry.
    #[serde(default)]
    remove_unused_wildcards: Option<bool>,
}

#[async_trait]
impl Tool for RustOrganizeImports {
    fn name(&self) -> &str {
        "rust.organizeImports"
    }
    fn description(&self) -> &str {
        "Minimize Rust wildcard imports (mode=\"minimize\", default): rewrite resolvable `use foo::*;` into explicit `use foo::{A, B};` for directly-referenced names. NEVER writes: returns {changes, creates, findings} for edits.merge/createFile. mode=\"organize\" (rust-analyzer source.organizeImports) lands with lsp.assist (phase 2)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (workspace-relative, no `..`)." },
                "mode": { "type": "string", "enum": ["minimize", "organize"], "description": "\"minimize\" (default) runs the syntactic wildcard-minimizer; \"organize\" (rust-analyzer source.organizeImports) lands with lsp.assist (phase 2)." },
                "allow_wildcards": { "type": "array", "items": { "type": "string" }, "description": "Optional allowlist of wildcard base paths to preserve verbatim." },
                "remove_unused_wildcards": { "type": "boolean", "description": "When true, wildcard imports with no directly-referenced names are deleted instead of left as leftovers." }
            },
            "required": ["source"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "organizeImports".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: OrganizeImportsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid input: {e}")),
        };
        let mode = match OrganizeMode::parse(params.mode.as_deref()) {
            Ok(mode) => mode,
            Err(e) => return ToolResult::Error(e),
        };
        if mode == OrganizeMode::Organize {
            return ToolResult::Error(
                "organize mode lands with lsp.assist (phase 2); use mode=minimize for the syntactic wildcard minimizer"
                    .to_string(),
            );
        }
        let root = cx.root.clone();
        // Sync fs + tree-sitter + planner work runs inside call_blocking, off
        // the tokio worker (concurrency-model section 5), matching every other
        // rust transform binding.
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || Self::run(params, mode, &root, &ledger)).await
    }
}

impl RustOrganizeImports {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker
    // (concurrency-model section 5).
    #[allow(clippy::disallowed_methods)]
    fn run(
        params: OrganizeImportsInput,
        mode: OrganizeMode,
        root: &std::path::Path,
        ledger: &ProvenanceLedger,
    ) -> ToolResult {
        let source_abs = match resolve_workspace_file(root, &params.source, "rust.organizeImports")
        {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.organizeImports: {e}")),
        };
        let mut entries = std::collections::BTreeMap::new();
        if let Some(allow) = params.allow_wildcards.as_ref()
            && !allow.is_empty()
        {
            entries.insert(
                "allow_wildcards".to_string(),
                Value::Array(allow.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }
        if let Some(true) = params.remove_unused_wildcards {
            entries.insert("remove_unused_wildcards".to_string(), Value::Bool(true));
        }
        let toml_entries = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
        let plan_params = RefactorPlanParams {
            kind: "rust_minimize_imports".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            toml_entries,
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&plan_params) {
            Ok(json) => json,
            Err(e) => {
                let hint = super::helpers::done_hint(&format!("{e:#}"));
                return ToolResult::Error(format!("rust.organizeImports: {e:#}{hint}"));
            }
        };
        let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
            Ok(plan) => plan,
            Err(e) => return ToolResult::Error(format!("rust.organizeImports: plan decode: {e}")),
        };
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(root, "rust.organizeImports", &plan.edits, false) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.organizeImports: {e}")),
        };
        record_in_ledger(ledger, "rust.organizeImports", &changes);
        let mut findings: Vec<Value> = Vec::new();
        for note in &plan.leftovers {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        ToolResult::Json(json!({
            "title": plan.title,
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "mode": mode.as_str(),
            "would_change_files": would_change_files,
            "would_create_files": would_create_files,
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

    /// A wildcard import whose target module is resolvable and whose names
    /// are directly referenced should minimize into an explicit use group.
    #[tokio::test]
    async fn minimize_expands_wildcard_into_explicit_use_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // The wildcard target resolves via the `crate::` prefix to
        // src/helper.rs (the v1 resolver locates `crate::helper` as
        // src/helper.rs from the project src dir).
        std::fs::write(
            src.join("helper.rs"),
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        )
        .unwrap();
        // `alpha` and `gamma` are referenced; `beta` is not, so the minimized
        // group names only the used ones.
        std::fs::write(
            src.join("lib.rs"),
            "mod helper;\nuse crate::helper::*;\n\npub fn call() {\n    alpha();\n    gamma();\n}\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustOrganizeImports(ledger());
        let result = tool.call(json!({ "source": "src/lib.rs" }), &cx).await;

        let val = match result {
            ToolResult::Json(v) => v,
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        };
        assert_eq!(val["mode"], "minimize");
        assert_eq!(val["provenance"], "syntax_only");
        let changes = val["changes"].as_array().expect("changes array");
        assert!(
            !changes.is_empty(),
            "minimize should propose at least one change"
        );
        // The replacement text must be an explicit use group naming the
        // referenced symbols and omitting the unused one.
        let replacement = changes
            .iter()
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            replacement.contains("use crate::helper::{") && replacement.contains("alpha"),
            "replacement should expand the wildcard into an explicit group naming alpha: {replacement}"
        );
        assert!(
            replacement.contains("gamma"),
            "replacement should name the other referenced symbol gamma: {replacement}"
        );
        assert!(
            !replacement.contains("beta"),
            "minimize should drop the unreferenced symbol beta: {replacement}"
        );
    }

    /// A file with no wildcard imports (or none that can be minimized) is a
    /// DONE-style result: the planner refuses, which surfaces as an error the
    /// cell treats as the no-op signal.
    #[tokio::test]
    async fn no_op_input_surfaces_done_style_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "use std::collections::HashMap;\n\npub fn build() -> HashMap<(), ()> {\n    HashMap::new()\n}\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustOrganizeImports(ledger());
        let result = tool.call(json!({ "source": "src/lib.rs" }), &cx).await;

        match result {
            // No wildcard imports: the planner bails with "no wildcard imports
            // found". This is the DONE signal, not a retryable failure.
            ToolResult::Error(e) => {
                assert!(
                    e.contains("no wildcard imports") || e.contains("could not be minimized"),
                    "expected a no-wildcards DONE-style error, got: {e}"
                );
            }
            other => panic!("expected error for no-op input, got: {other:?}"),
        }
    }

    /// `mode="organize"` must refuse with the phase-2 pointer, never stub a
    /// fake organize.
    #[tokio::test]
    async fn organize_mode_refuses_with_phase_two_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "use std::io::*;\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustOrganizeImports(ledger());
        let result = tool
            .call(json!({ "source": "src/lib.rs", "mode": "organize" }), &cx)
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(
                    e.contains("organize mode lands with lsp.assist (phase 2)"),
                    "expected the phase-2 organize refusal, got: {e}"
                );
                assert!(
                    e.contains("mode=minimize"),
                    "refusal should point at the minimize alternative: {e}"
                );
            }
            other => panic!("expected error for organize mode, got: {other:?}"),
        }
    }

    /// An unknown mode value is rejected up front with an actionable error.
    #[tokio::test]
    async fn unknown_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "use std::io::*;\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustOrganizeImports(ledger());
        let result = tool
            .call(json!({ "source": "src/lib.rs", "mode": "reorder" }), &cx)
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(
                    e.contains("mode must be") && e.contains("reorder"),
                    "expected a mode-validation error naming the bad value, got: {e}"
                );
            }
            other => panic!("expected error for unknown mode, got: {other:?}"),
        }
    }
}

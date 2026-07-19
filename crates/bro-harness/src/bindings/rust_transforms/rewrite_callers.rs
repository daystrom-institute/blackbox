//! `rust.rewriteModuleCallers` - rewrite caller prefixes after a module move.
//!
//! Port of the caller-prefix rewrite half of v1 `move_rust_items_with_callers`
//! from `bbox_refactor::rust_move_with_callers`, decomposed per design §3.1.
//! For each named moved item, rewrites every `<source_simple>::<item>`
//! occurrence in other project .rs files to `<target_simple>::<item>`,
//! including inside `use` declarations. Word-boundary checked.
//!
//! NEVER writes. Returns `{changes, findings, counts}` for `edits.merge`.
//!
//! Known v1 limits (documented, not fixed):
//! - Simple-name segment match only. `crate::foo::source_simple::Item` gets
//!   rewritten; `crate::foo::Item` (where the canonical path skipped
//!   `source_simple`) does not.
//! - No splitting of multi-import use trees.
//! - No alias awareness. `use foo::Item as X;` works the same as non-aliased.

use std::collections::HashSet;
use std::fs;

use async_trait::async_trait;
use bbox_refactor::{
    TextEdit, path_string, rust_move_with_callers, sha256_hex,
};
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

/// `rust.rewriteModuleCallers` tool.
pub struct RustRewriteModuleCallers;

#[derive(Deserialize)]
struct RewriteModuleCallersInput {
    /// Project directory for the caller walk (relative to worktree root, or
    /// absolute). Skips `target/`, `build/`, `node_modules/`, `.git/`.
    project_dir: String,
    /// Names of the moved items (required, non-empty).
    item_names: Vec<String>,
    /// Source module's simple name. Defaults to the source file stem (with
    /// `lib`/`main`/`mod` rejected — explicit override required).
    #[serde(default)]
    module_name: Option<String>,
    /// Target module's simple name. Defaults to the target file stem;
    /// overrides `target_prelude`-as-module-name convention from v1.
    #[serde(default)]
    target_prelude: Option<String>,
    /// Optional list of file paths that are the source and target of the move.
    /// These files are skipped during the walk (they're already covered by
    /// the extract/move itself).
    #[serde(default)]
    skip_files: Option<Vec<String>>,
}

fn resolve_path(root: &std::path::Path, rel: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn text_edits_to_span_changes(
    edits: &[TextEdit],
    file_path: &str,
    content_sha256: &str,
) -> Vec<Value> {
    edits
        .iter()
        .map(|edit| {
            json!({
                "span": {
                    "file": file_path,
                    "byte_start": edit.byte_start,
                    "byte_end": edit.byte_end,
                    "content_sha256": content_sha256
                },
                "new_text": edit.replacement
            })
        })
        .collect()
}

#[async_trait]
impl Tool for RustRewriteModuleCallers {
    fn name(&self) -> &str {
        "rust.rewriteModuleCallers"
    }
    fn description(&self) -> &str {
        "Rewrite caller prefixes after a module move. For each named moved item, rewrites every <source_simple>::<item> in other project .rs files to <target_simple>::<item>, including inside use declarations. Word-boundary checked. NEVER writes: returns {changes, findings, counts} for edits.merge."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_dir": { "type": "string", "description": "Project directory for the caller walk. Skips target/, build/, node_modules/, .git/." },
                "item_names": { "type": "array", "items": {"type": "string"}, "description": "Names of the moved items (required, non-empty)." },
                "module_name": { "type": "string", "description": "Source module's simple name. Defaults to the source file stem (lib/main/mod rejected without explicit override)." },
                "target_prelude": { "type": "string", "description": "Target module's simple name. Defaults to the target file stem." },
                "skip_files": { "type": "array", "items": {"type": "string"}, "description": "File paths to skip during the walk (source/target of the extract/move)." }
            },
            "required": ["project_dir", "item_names"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "rewriteModuleCallers".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: RewriteModuleCallersInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid input: {e}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error(
                "item_names is required and must not be empty".to_string(),
            );
        }
        let root = cx.root.clone();
        // Sync fs + walk work runs inside call_blocking, off the tokio worker
        // (concurrency-model section 5), matching every other binding.
        bro_tools::tool::call_blocking(move || Self::run(params, &root)).await
    }
}

impl RustRewriteModuleCallers {
    // Sync fs access is sanctioned here: callers run inside the call_blocking
    // closure of this binding tool, never on a tokio worker
    // (concurrency-model section 5).
    #[allow(clippy::disallowed_methods)]
    fn run(params: RewriteModuleCallersInput, root: &std::path::Path) -> ToolResult {
        let project_dir = resolve_path(root, &params.project_dir);
        if !project_dir.is_dir() {
            return ToolResult::Error(format!(
                "project_dir is not a directory: {}",
                project_dir.display()
            ));
        }

        // Resolve module simple names. For rewrites, we don't have source/target
        // file paths — the caller must provide module_name and target_prelude
        // explicitly, or we derive from source/target file stems.
        // In the decomposed model, the caller supplies the module names directly
        // because there's no attached source/target file geometry.
        let source_simple = params
            .module_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let target_simple = params
            .target_prelude
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let (source_simple, target_simple) = match (source_simple, target_simple) {
            (Some(src), Some(tgt)) => {
                if src == tgt {
                    return ToolResult::Error(format!(
                        "source and target module simple names must differ; got `{src}`"
                    ));
                }
                (src, tgt)
            }
            _ => {
                return ToolResult::Error(
                    "module_name and target_prelude are required for rewriteModuleCallers \
                     (in the decomposed model there is no attached source/target file geometry; \
                      provide explicit module names)"
                        .to_string(),
                );
            }
        };

        let moved_names: HashSet<&str> =
            params.item_names.iter().map(String::as_str).collect();

        // Resolve canonical skip paths.
        let skip_canonical: Vec<_> = params
            .skip_files
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|f| {
                let p = resolve_path(root, f);
                std::fs::canonicalize(&p).unwrap_or(p)
            })
            .collect();

        let mut all_changes: Vec<Value> = Vec::new();
        let mut files_touched = 0u64;
        let mut total_rewrites = 0u64;
        let mut findings: Vec<Value> = Vec::new();

        for entry in walkdir::WalkDir::new(&project_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            // Skip well-known build/dependency directories.
            if path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                )
            }) {
                continue;
            }
            // Skip explicitly listed files.
            let canonical = std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
            if skip_canonical.contains(&canonical) {
                continue;
            }

            let caller_source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let edits = rust_move_with_callers::compute_caller_rewrite_edits(
                &caller_source,
                &source_simple,
                &target_simple,
                &moved_names,
            );
            if edits.is_empty() {
                continue;
            }
            let sha = sha256_hex(caller_source.as_bytes());
            let changes = text_edits_to_span_changes(&edits, &path_string(path), &sha);
            total_rewrites += changes.len() as u64;
            files_touched += 1;
            all_changes.extend(changes);

            // Bound payload per isolate-heap discipline.
            if all_changes.len() > 2000 {
                findings.push(json!({
                    "finding": "rewrite_cap_reached",
                    "files_touched": files_touched,
                    "rewrites": total_rewrites,
                    "detail": "rewriteModuleCallers hit the per-call change cap (2000); narrow the file set and re-run"
                }));
                break;
            }
        }

        ToolResult::Json(json!({
            "changes": all_changes,
            "findings": findings,
            "counts": {
                "files_touched": files_touched,
                "rewrites": total_rewrites
            },
            "provenance": "syntax_only"
        }))
    }
}

#[cfg(test)]
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

    #[tokio::test]
    async fn rewrites_callers_in_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("mod_a.rs"), "pub fn moved() -> usize { 1 }\n").unwrap();
        fs::write(src_dir.join("mod_b.rs"), "").unwrap();
        fs::write(
            src_dir.join("usage.rs"),
            "use crate::mod_a::moved;\nfn run() { let _ = moved(); }\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"],
                    "module_name": "mod_a",
                    "target_prelude": "mod_b",
                    "skip_files": ["src/mod_a.rs", "src/mod_b.rs"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                assert_eq!(changes.len(), 1, "expected 1 change (the use rewrite)");
                let new_text = changes[0]["new_text"].as_str().unwrap();
                assert_eq!(new_text, "mod_b", "prefix should be rewritten to mod_b");
                let counts = &val["counts"];
                assert_eq!(counts["files_touched"], 1);
                assert_eq!(counts["rewrites"], 1);
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_word_boundary_false_positives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("mod_a.rs"), "pub fn moved() {}\n").unwrap();
        fs::write(src_dir.join("mod_b.rs"), "").unwrap();
        // mod_ax::moved should NOT match mod_a::moved.
        fs::write(src_dir.join("other.rs"), "use crate::mod_ax::moved;\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"],
                    "module_name": "mod_a",
                    "target_prelude": "mod_b",
                    "skip_files": ["src/mod_a.rs", "src/mod_b.rs"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                assert!(changes.is_empty(), "false-positive prefix should not produce changes");
                let counts = &val["counts"];
                assert_eq!(counts["files_touched"], 0);
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rewrites_bare_path_expressions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("mod_a.rs"), "pub fn moved() {}\n").unwrap();
        fs::write(src_dir.join("mod_b.rs"), "").unwrap();
        fs::write(
            src_dir.join("caller.rs"),
            "fn run() {\n    let _ = mod_a::moved();\n    let _ = crate::mod_a::moved;\n}\n",
        )
        .unwrap();

        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"],
                    "module_name": "mod_a",
                    "target_prelude": "mod_b",
                    "skip_files": ["src/mod_a.rs", "src/mod_b.rs"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                // Two rewrites: mod_a::moved() and crate::mod_a::moved
                assert_eq!(changes.len(), 2, "expected 2 rewrites, got {changes:?}");
                let counts = &val["counts"];
                assert_eq!(counts["files_touched"], 1);
                assert_eq!(counts["rewrites"], 2);
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn skips_excluded_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        let build_dir = root.join("target").join("debug").join("build");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::write(src_dir.join("mod_a.rs"), "pub fn moved() {}\n").unwrap();
        fs::write(src_dir.join("mod_b.rs"), "").unwrap();
        fs::write(build_dir.join("artifact.rs"), "use crate::mod_a::moved;\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"],
                    "module_name": "mod_a",
                    "target_prelude": "mod_b",
                    "skip_files": ["src/mod_a.rs", "src/mod_b.rs"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                assert!(changes.is_empty(), "build-dir file should be skipped");
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_missing_module_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("mod_a.rs"), "pub fn moved() {}\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(
                    e.contains("module_name"),
                    "error should mention module_name, got: {e}"
                );
            }
            _ => panic!("expected error, got {result:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_same_source_and_target_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cx = cx_in(&root);
        let tool = RustRewriteModuleCallers;
        let result = tool
            .call(
                json!({
                    "project_dir": ".",
                    "item_names": ["moved"],
                    "module_name": "same",
                    "target_prelude": "same"
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(e.contains("must differ"), "error should mention names differ");
            }
            _ => panic!("expected error, got {result:?}"),
        }
    }
}

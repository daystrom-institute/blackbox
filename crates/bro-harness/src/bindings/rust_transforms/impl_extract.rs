//! `rust.extractImplMethods` - extract named impl methods into another file.
//!
//! Port of v1 `extract_rust_impl_methods` from `bbox_refactor::rust`.
//! Moves named methods out of one `impl` block into another file, preserving
//! attributes/modifiers (async etc.), rebasing `super::` paths one module
//! deeper when the target is a child module, and applying visibility overrides.
//!
//! rmcp `tool_router` wrapper generation is NOT ported (repo-specific; out of
//! scope per design §3.1).
//!
//! NEVER writes. Returns `{changes, creates, findings, leftovers}` for
//! `edits.merge` + `edits.createFile`.

use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use bbox_refactor::{
    RustImplMethod, TextEdit, apply_text_edits, ensure_non_overlapping, parse_rust_file,
    path_string, rust_impl_methods, rust_impl_methods_target_edits, sha256_hex,
};
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

/// `rust.extractImplMethods` tool.
pub struct RustExtractImplMethods;

#[derive(Deserialize)]
struct ExtractImplMethodsInput {
    /// Source file path (relative to worktree root, or absolute).
    source: String,
    /// Target file path.
    target: String,
    /// Names of the impl methods to move (required, non-empty).
    item_names: Vec<String>,
    /// Optional impl block name disambiguator (the type name in `impl Foo`).
    #[serde(default)]
    impl_name: Option<String>,
    /// Optional explicit visibility for every moved method. When set, every
    /// moved method gets this visibility regardless of original. When unset,
    /// previously-private methods widen to `pub(super)` only when the parent
    /// still references them; existing pub/pub(crate)/pub(super) are preserved.
    #[serde(default)]
    visibility: Option<String>,
    /// Optional target prelude (e.g. `use` statements) inserted after
    /// shebang / inner attrs / inner doc comments in the target file.
    #[serde(default)]
    target_prelude: Option<String>,
}

fn resolve_path(root: &std::path::Path, rel: &str) -> PathBuf {
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

fn rust_text_contains_identifier(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || needle_bytes.len() > bytes.len() {
        return false;
    }
    bytes
        .windows(needle_bytes.len())
        .enumerate()
        .any(|(idx, window)| {
            window == needle_bytes
                && !is_ident_byte(bytes.get(idx.wrapping_sub(1)).copied())
                && !is_ident_byte(bytes.get(idx + needle_bytes.len()).copied())
        })
}

fn is_ident_byte(ch: Option<u8>) -> bool {
    matches!(ch, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn rust_target_is_child_module_of_source(
    source_path: &std::path::Path,
    target_path: &std::path::Path,
) -> bool {
    let Some(source_parent) = source_path.parent() else {
        return false;
    };
    let Some(source_stem) = source_path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    target_path.parent() == Some(&source_parent.join(source_stem))
}

#[async_trait]
impl Tool for RustExtractImplMethods {
    fn name(&self) -> &str {
        "rust.extractImplMethods"
    }
    fn description(&self) -> &str {
        "Move named Rust impl methods from one file into another. Preserves attributes/modifiers, rebases super:: paths one module deeper, applies visibility overrides. NEVER writes: returns {changes, creates, findings, leftovers} for edits.merge + edits.createFile."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file path (relative to worktree root, or absolute)." },
                "target": { "type": "string", "description": "Target file path." },
                "item_names": { "type": "array", "items": {"type": "string"}, "description": "Names of the impl methods to move (required, non-empty)." },
                "impl_name": { "type": "string", "description": "Optional impl block name disambiguator (the type name in `impl Foo`)." },
                "visibility": { "type": "string", "description": "Optional explicit visibility for every moved method." },
                "target_prelude": { "type": "string", "description": "Optional target prelude (e.g. use statements) inserted after shebang/inner attrs/doc comments." }
            },
            "required": ["source", "target", "item_names"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "extractImplMethods".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ExtractImplMethodsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("invalid input: {e}")),
        };
        if params.item_names.is_empty() {
            return ToolResult::Error("item_names is required and must not be empty".to_string());
        }
        let root = cx.root.clone();
        // Sync fs + tree-sitter work runs inside call_blocking, off the tokio
        // worker (concurrency-model section 5), matching every other binding.
        bro_tools::tool::call_blocking(move || Self::run(params, &root)).await
    }
}

impl RustExtractImplMethods {
    fn run(params: ExtractImplMethodsInput, root: &std::path::Path) -> ToolResult {
        let source_path = resolve_path(root, &params.source);
        let target_path = resolve_path(root, &params.target);
        if source_path == target_path {
            return ToolResult::Error("source and target must be different files".to_string());
        }

        // Parse source.
        let parsed = match parse_rust_file(&source_path) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("parse source: {e:#}")),
        };
        let source_sha = sha256_hex(parsed.source.as_bytes());

        // Find and select methods.
        let methods = rust_impl_methods(&parsed);
        let candidates: Vec<&RustImplMethod> = methods
            .iter()
            .filter(|method| {
                params
                    .impl_name
                    .as_deref()
                    .is_none_or(|impl_name| method.impl_name == impl_name)
            })
            .collect();
        if candidates.is_empty() {
            if let Some(impl_name) = params.impl_name.as_deref() {
                return ToolResult::Error(format!(
                    "no impl block matching `{impl_name}` found"
                ));
            }
            return ToolResult::Error("no Rust impl methods found".to_string());
        }

        let mut selected: Vec<RustImplMethod> = Vec::new();
        for expected in &params.item_names {
            let matches: Vec<&&RustImplMethod> = candidates
                .iter()
                .filter(|method| method.item.name.as_deref() == Some(expected.as_str()))
                .collect();
            match matches.as_slice() {
                [] => {
                    return ToolResult::Error(format!(
                        "requested impl method `{expected}` was not found"
                    ));
                }
                [method] => selected.push((**method).clone()),
                _ => {
                    return ToolResult::Error(format!(
                        "requested impl method `{expected}` matched multiple impl blocks; pass impl_name"
                    ));
                }
            }
        }

        // All selected methods must be from the same impl block.
        let impl_starts: std::collections::HashSet<_> =
            selected.iter().map(|m| m.impl_byte_start).collect();
        if impl_starts.len() > 1 {
            return ToolResult::Error(
                "extractImplMethods can only extract methods from one impl block per call"
                    .to_string(),
            );
        }

        let selected_ids: std::collections::HashSet<_> = selected
            .iter()
            .map(|method| method.item.plan_local_id.clone())
            .collect();
        let leftovers: Vec<String> = methods
            .iter()
            .filter(|method| !selected_ids.contains(&method.item.plan_local_id))
            .map(|method| {
                format!(
                    "impl_method {} in {} bytes {}..{}",
                    method.item.name.as_deref().unwrap_or("(unnamed)"),
                    method.impl_name,
                    method.item.byte_start,
                    method.item.byte_end
                )
            })
            .collect();

        // Source edits: delete the selected methods. Sort by byte_start and
        // merge adjacent/overlapping edits (adjacent methods share trivia
        // ranges when tree-sitter's leading_trivia_start / trailing_trivia_end
        // of consecutive items touch or overlap).
        let mut source_edits: Vec<TextEdit> = selected
            .iter()
            .map(|method| TextEdit {
                byte_start: method.item.leading_trivia_start,
                byte_end: method.item.trailing_trivia_end,
                replacement: String::new(),
            })
            .collect();
        source_edits.sort_by_key(|e| e.byte_start);
        let mut merged: Vec<TextEdit> = Vec::new();
        for edit in source_edits {
            if let Some(last) = merged.last_mut() {
                if edit.byte_start <= last.byte_end {
                    // Overlapping or touching — merge into one span.
                    last.byte_end = last.byte_end.max(edit.byte_end);
                    continue;
                }
            }
            merged.push(edit);
        }
        if let Err(e) = ensure_non_overlapping(&merged) {
            return ToolResult::Error(format!("source edits overlap after merge: {e:#}"));
        }
        let source_edits = merged;

        // Check if parent still references moved methods after deletion.
        let parent_after_move = match apply_text_edits(&parsed.source, &source_edits) {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(format!("apply source edits: {e:#}")),
        };
        let parent_still_calls = selected
            .iter()
            .filter_map(|method| method.item.name.as_deref())
            .any(|name| rust_text_contains_identifier(&parent_after_move, name));

        let explicit_visibility = params.visibility.as_deref();
        let fallback_visibility =
            if explicit_visibility.is_none() && parent_still_calls {
                Some("pub(super)")
            } else {
                None
            };

        let rebase_super_paths =
            rust_target_is_child_module_of_source(&source_path, &target_path);

        // Read target and compute target edits.
        let target_source = fs::read_to_string(&target_path).unwrap_or_default();
        let target_sha = sha256_hex(target_source.as_bytes());
        let target_edits = match rust_impl_methods_target_edits(
            &target_path,
            &target_source,
            params.target_prelude.as_deref(),
            None, // router_name: NOT ported
            None, // router_export_name: NOT ported
            &selected[0].impl_name,
            &parsed.source,
            &selected,
            explicit_visibility,
            fallback_visibility,
            rebase_super_paths,
        ) {
            Ok(edits) => edits,
            Err(e) => return ToolResult::Error(format!("target edits: {e:#}")),
        };

        // Convert to binding shape.
        let mut changes: Vec<Value> = Vec::new();
        changes.extend(text_edits_to_span_changes(
            &source_edits,
            &path_string(&source_path),
            &source_sha,
        ));
        changes.extend(text_edits_to_span_changes(
            &target_edits,
            &path_string(&target_path),
            &target_sha,
        ));

        // Track if target is new (empty or non-existent).
        let target_is_new = target_source.trim().is_empty();
        let creates: Vec<Value> = if target_is_new {
            // The target edits already contain the full replacement (including
            // prelude). We report creates so the cell can use edits.createFile.
            let applied = match apply_text_edits(&target_source, &target_edits) {
                Ok(s) => s,
                Err(e) => return ToolResult::Error(format!("apply target edits: {e:#}")),
            };
            vec![json!({
                "path": path_string(&target_path),
                "content": applied
            })]
        } else {
            Vec::new()
        };

        let counts = json!({
            "moved": selected.len(),
            "leftovers": leftovers.len()
        });

        ToolResult::Json(json!({
            "changes": changes,
            "creates": creates,
            "findings": [],
            "leftovers": leftovers,
            "counts": counts,
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
    async fn extracts_simple_impl_method() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("foo.rs"),
            "pub struct Foo;\n\nimpl Foo {\n    pub fn hello(&self) -> &str {\n        \"hello\"\n    }\n}\n",
        )
        .unwrap();
        fs::write(src_dir.join("bar.rs"), "").unwrap();

        let cx = cx_in(&root);
        let tool = RustExtractImplMethods;
        let result = tool
            .call(
                json!({
                    "source": "src/foo.rs",
                    "target": "src/bar.rs",
                    "item_names": ["hello"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                assert!(!changes.is_empty(), "expected changes");
                let leftovers = val["leftovers"].as_array().expect("leftovers array");
                assert!(leftovers.is_empty(), "expected no leftovers, got {leftovers:?}");
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn visibility_fallback_pub_super_when_parent_still_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("foo.rs"),
            "pub struct Foo;\n\nimpl Foo {\n    fn helper(&self) -> bool { true }\n\n    pub fn entry(&self) -> bool { self.helper() }\n}\n",
        )
        .unwrap();
        fs::write(src_dir.join("bar.rs"), "").unwrap();

        let cx = cx_in(&root);
        let tool = RustExtractImplMethods;
        let result = tool
            .call(
                json!({
                    "source": "src/foo.rs",
                    "target": "src/bar.rs",
                    "item_names": ["helper"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                let target_change = changes
                    .iter()
                    .find(|c| !c["new_text"].as_str().unwrap_or("").is_empty())
                    .expect("target insert change");
                let new_text = target_change["new_text"].as_str().unwrap();
                assert!(
                    new_text.contains("pub(super) fn helper"),
                    "private method should widen to pub(super), got: {new_text}"
                );
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_visibility_overrides_all() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("foo.rs"),
            "pub struct Foo;\n\nimpl Foo {\n    pub fn hello(&self) {}\n    fn helper(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(src_dir.join("bar.rs"), "").unwrap();

        let cx = cx_in(&root);
        let tool = RustExtractImplMethods;
        let result = tool
            .call(
                json!({
                    "source": "src/foo.rs",
                    "target": "src/bar.rs",
                    "item_names": ["hello", "helper"],
                    "visibility": "pub(crate)"
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Json(val) => {
                let changes = val["changes"].as_array().expect("changes array");
                let target_change = changes
                    .iter()
                    .find(|c| !c["new_text"].as_str().unwrap_or("").is_empty())
                    .expect("target insert change");
                let new_text = target_change["new_text"].as_str().unwrap();
                assert!(
                    new_text.contains("pub(crate) fn hello"),
                    "explicit visibility should apply to all methods, got: {new_text}"
                );
                assert!(
                    new_text.contains("pub(crate) fn helper"),
                    "explicit visibility should apply to private methods too, got: {new_text}"
                );
            }
            ToolResult::Error(e) => panic!("unexpected error: {e}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_items_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("foo.rs"), "impl Foo { fn a(&self) {} }\n").unwrap();

        let cx = cx_in(&root);
        let tool = RustExtractImplMethods;
        let result = tool
            .call(
                json!({
                    "source": "src/foo.rs",
                    "target": "src/bar.rs",
                    "item_names": ["missing"]
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(e.contains("missing"), "error should name the missing method");
            }
            _ => panic!("expected error, got {result:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_item_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cx = cx_in(&root);
        let tool = RustExtractImplMethods;
        let result = tool
            .call(
                json!({
                    "source": "src/foo.rs",
                    "target": "src/bar.rs",
                    "item_names": []
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(e.contains("not be empty"), "error should mention empty names");
            }
            _ => panic!("expected error, got {result:?}"),
        }
    }
}

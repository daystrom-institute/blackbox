//! Gap 4: cross-file static caller rewriter for `extract_java_class`.
//!
//! When a static method or constant moves from `OldOwnerClass` (in the source
//! file) to `NewOwnerClass` (in the target file, possibly a different
//! package), callers in OTHER project files still reference the symbol via
//! the old qualifier. This module walks every `.java` file under
//! `project_dir`, finds `OldOwnerClass.<symbol>` references (method
//! invocations, field accesses, method references), and emits the qualifier
//! rewrite + import injection needed to keep the project linkable.
//!
//! Carved out of `src/refactor/java.rs` to reduce that file's footprint —
//! the functions here are called only from `extract_java_class`.

use super::*;

/// Gap 4: a moved item whose cross-file `OldOwner.<item>` references need
/// rewriting after `extract_java_class` runs. Only static members qualify
/// for the cross-file pass — instance methods still resolve through the
/// source-side delegate field, which is private and unreachable from
/// other files (a future iteration could surface a forwarding-method
/// advisory for instance calls).
pub(super) struct MovedStaticItem {
    pub(super) name: String,
    /// `"method"` for static method calls, `"field"` for static-final
    /// constant accesses. Drives which AST node shapes we match.
    pub(super) kind: &'static str,
}


/// Gap 4: compute caller-rewrite FileEdits for every `.java` file in the
/// project that references `<old_class>.<moved_item>` as a static call or
/// field access. The source file and the target file are excluded — both
/// already have edits computed via the in-file rewriter and the target
/// renderer respectively. Files under `target/`, `build/`, `.gradle/`,
/// `node_modules/`, and `.git/` are skipped (build outputs, vendored
/// trees).
pub(super) fn compute_cross_file_static_caller_edits(
    project_dir: &Path,
    source_path: &Path,
    target_path: &Path,
    old_class: &str,
    new_class: &str,
    target_package: Option<&str>,
    moved_items: &[MovedStaticItem],
) -> Vec<FileEdit> {
    if moved_items.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let canonical_source = fs::canonicalize(source_path).ok();
    let canonical_target = fs::canonicalize(target_path).ok();
    let method_names: HashSet<&str> = moved_items
        .iter()
        .filter(|m| m.kind == "method")
        .map(|m| m.name.as_str())
        .collect();
    let field_names: HashSet<&str> = moved_items
        .iter()
        .filter(|m| m.kind == "field")
        .map(|m| m.name.as_str())
        .collect();
    for entry in walkdir::WalkDir::new(project_dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("java") {
            continue;
        }
        if path.components().any(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some("target" | "build" | ".gradle" | "node_modules" | ".git")
            )
        }) {
            continue;
        }
        let canonical_path = fs::canonicalize(path).ok();
        if canonical_path.is_some()
            && (canonical_path == canonical_source || canonical_path == canonical_target)
        {
            continue;
        }
        let Ok(parsed) = parse_source_file(path) else {
            continue;
        };
        if parsed.language != "java" {
            continue;
        }
        let qualifier_edits = compute_static_qualifier_rewrite_edits(
            &parsed,
            old_class,
            new_class,
            &method_names,
            &field_names,
        );
        if qualifier_edits.is_empty() {
            continue;
        }
        let mut all_edits = qualifier_edits;
        if let Some(target_pkg) = target_package {
            let fqcn = if target_pkg.is_empty() {
                new_class.to_string()
            } else {
                format!("{target_pkg}.{new_class}")
            };
            if let Some(import_edit) = java_source_import_edit(&parsed.source, &fqcn) {
                all_edits.push(import_edit);
            }
        }
        all_edits.sort_by_key(|e| e.byte_start);
        out.push(FileEdit {
            path: path_string(path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: all_edits,
            new_text: None,
        });
    }
    out
}


/// Scan one parsed Java file for `<old_class>.<name>` references where
/// `<name>` is a moved method or field. Emits a TextEdit per match that
/// rewrites the `<old_class>` identifier in the qualifier slot to
/// `<new_class>`. The dotted name and arg list / suffix stay intact.
pub(super) fn compute_static_qualifier_rewrite_edits(
    parsed: &ParsedSource,
    old_class: &str,
    new_class: &str,
    method_names: &HashSet<&str>,
    field_names: &HashSet<&str>,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let source_bytes = parsed.source.as_bytes();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "method_invocation" => {
                if let (Some(object), Some(name)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("name"),
                ) {
                    if object.kind() == "identifier"
                        && object.utf8_text(source_bytes).ok() == Some(old_class)
                    {
                        if let Ok(name_text) = name.utf8_text(source_bytes) {
                            if method_names.contains(name_text) {
                                edits.push(TextEdit {
                                    byte_start: object.start_byte(),
                                    byte_end: object.end_byte(),
                                    replacement: new_class.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            "field_access" => {
                // `OldClass.PROTREND` parses as field_access; the qualifier
                // is the `object` child (an identifier) and the constant
                // name is the `field` child (an identifier).
                if let (Some(object), Some(field)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("field"),
                ) {
                    if object.kind() == "identifier"
                        && object.utf8_text(source_bytes).ok() == Some(old_class)
                    {
                        if let Ok(field_text) = field.utf8_text(source_bytes) {
                            if field_names.contains(field_text) {
                                edits.push(TextEdit {
                                    byte_start: object.start_byte(),
                                    byte_end: object.end_byte(),
                                    replacement: new_class.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            "method_reference" => {
                // `OldClass::method` — qualifier is the first named child.
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                if children.len() == 2 {
                    let qualifier = children[0];
                    let name_node = children[1];
                    if qualifier.kind() == "identifier"
                        && qualifier.utf8_text(source_bytes).ok() == Some(old_class)
                        && name_node.kind() == "identifier"
                    {
                        if let Ok(name_text) = name_node.utf8_text(source_bytes) {
                            if method_names.contains(name_text) {
                                edits.push(TextEdit {
                                    byte_start: qualifier.start_byte(),
                                    byte_end: qualifier.end_byte(),
                                    replacement: new_class.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    edits.sort_by_key(|e| e.byte_start);
    edits
}

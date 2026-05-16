//! `extract_java_test_slice` — move `@Test` methods from a source test
//! class to a target test class, following an `extract_java_class`
//! production move.
//!
//! v1 scope: test migration only. Walks `@Test` methods in the source
//! test class, classifies each by whether it exercises moved methods
//! (`item_names`), kept methods, or both. Moves "only moved" tests to
//! the target; leaves "only kept" tests alone; refuses to split
//! "mixed" tests automatically.
//!
//! Mockito stub synthesis (`@Mock` + `when().thenReturn()` boilerplate
//! in remaining-source tests) is filed as a v2 follow-up — that half
//! needs to detect MockitoExtension, generate setup code, and rewrite
//! direct calls into stubbed-mock calls.
//!
//! ## Inputs
//!
//! - `source` (required) — path to the source test class file
//!   (the test for the production class that just got extracted).
//! - `target` (required) — path to the target test class file. May
//!   not exist; planner creates it with package + imports from source.
//! - `project_dir` (required)
//! - `item_names` (required) — list of moved production method names.
//!   Tests whose `@Test` bodies invoke ONLY these methods (no other
//!   production method) are migrated to the target.
//! - `module_name` (optional) — target test class name. Default:
//!   derived from `target` filename.
//! - `impl_name` (optional) — source test class name (when source has
//!   multiple top-level classes).
//!
//! ## Refusals
//!
//! - `error.no_test_methods_in_source`
//! - `error.no_tests_to_move` — every @Test in source exercises kept
//!   methods (none qualifies for migration).
//! - `error.mixed_coverage_test` — at least one test exercises BOTH
//!   moved and kept methods; the operator must split the test
//!   manually (e.g. via `extract_java_methods` on the test class
//!   first) before re-running.
//! - `error.target_already_exists`

use super::*;
use std::collections::HashSet;

pub(crate) fn plan_extract_java_test_slice(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_test_slice"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if target_path.exists() {
        bail!(
            "target {} already exists; pick a different target or remove first",
            target_path.display()
        );
    }

    let moved_names: HashSet<String> = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "item_names (list of moved production method names) is required for \
                 extract_java_test_slice"
            )
        })?
        .iter()
        .cloned()
        .collect();

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_test_slice only supports java files");
    }

    let class_node = if let Some(class_name) = p.impl_name.as_deref() {
        find_class_declaration_by_name(&parsed, class_name).ok_or_else(|| {
            anyhow!(
                "class `{class_name}` not found in {}",
                source_path.display()
            )
        })?
    } else {
        find_first_class_declaration(parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?
    };
    let source_class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .unwrap_or("(unnamed)")
        .to_string();

    // Enumerate @Test methods.
    let tests = collect_test_methods(class_node, &parsed.source);
    if tests.is_empty() {
        bail!(
            "no @Test methods found in {} (class `{source_class_name}`)",
            source_path.display()
        );
    }

    // Classify each test by which moved/kept production methods it
    // exercises. "Production method" heuristic for v1: any bare
    // method_invocation (no receiver, no `this.` prefix) whose .name
    // matches an identifier that the operator-supplied `item_names`
    // either includes (= moved) or doesn't include (= kept).
    let mut to_move: Vec<&TestMethod> = Vec::new();
    let mut mixed: Vec<String> = Vec::new();
    for test in &tests {
        let invocations = collect_bare_invocations(test.node, &parsed.source);
        let touches_moved = invocations.iter().any(|n| moved_names.contains(n));
        let touches_kept = invocations.iter().any(|n| !moved_names.contains(n));
        match (touches_moved, touches_kept) {
            (true, false) => to_move.push(test),
            (true, true) => mixed.push(test.name.clone()),
            _ => {}
        }
    }
    if !mixed.is_empty() {
        bail!(
            "extract_java_test_slice: test method(s) exercise both moved and kept production \
             methods: {} — split each into single-coverage tests before re-running",
            mixed.join(", ")
        );
    }
    if to_move.is_empty() {
        bail!(
            "no @Test methods in {} exercise the moved production methods; nothing to migrate",
            source_path.display()
        );
    }

    let target_class_name = p
        .module_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            target_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("TargetTest")
                .to_string()
        });

    // Build the target file's body — package + imports from source +
    // skeleton test class with the moved methods.
    let target_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)
        .ok()
        .flatten();
    let prelude = java_default_target_prelude(p, &parsed.source, target_pkg.as_deref());

    let body_blocks: Vec<String> = to_move
        .iter()
        .map(|t| reindent_method_to_block(&parsed.source, t.byte_start, t.byte_end))
        .collect();
    let body_joined = body_blocks.join("\n\n");
    let target_text = format!(
        "{prelude}public class {target_class_name} {{\n{body_joined}\n}}\n"
    );

    // Build the source-side delete edits for migrated tests (descending
    // so byte indices don't shift).
    let mut source_edits: Vec<TextEdit> = to_move
        .iter()
        .map(|t| TextEdit {
            byte_start: leading_trivia(t.byte_start, &parsed.source),
            byte_end: trailing_newline(t.byte_end, &parsed.source),
            replacement: String::new(),
        })
        .collect();
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;

    let moved_names_list = to_move
        .iter()
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join(", ");

    let plan = RefactorPlan {
        title: format!(
            "extract {} test method(s) from {} into {}",
            to_move.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "extract_java_test_slice".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(b""),
                edits: vec![TextEdit {
                    byte_start: 0,
                    byte_end: 0,
                    replacement: String::new(),
                }],
                new_text: Some(target_text),
            },
        ],
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: Vec::new(),
        leftovers: vec![format!("migrated_tests={moved_names_list}")],
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

struct TestMethod<'a> {
    node: Node<'a>,
    name: String,
    byte_start: usize,
    byte_end: usize,
}

fn collect_test_methods<'a>(class_node: Node<'a>, source: &str) -> Vec<TestMethod<'a>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        if !has_annotation_in(child, source, "Test") {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        out.push(TestMethod {
            node: child,
            name: name.to_string(),
            byte_start: child.start_byte(),
            byte_end: child.end_byte(),
        });
    }
    out
}

fn has_annotation_in(node: Node<'_>, source: &str, annotation: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            let mk = mod_child.kind();
            if mk == "marker_annotation" || mk == "annotation" {
                if let Some(name_node) = mod_child.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(bytes) {
                        if name == annotation {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Walk the test method's body for bare `method_invocation` calls
/// (no receiver, or `this`-only receiver). Returns the list of called
/// names.
fn collect_bare_invocations(test_method: Node<'_>, source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![test_method];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        // Only bare calls (no object) or `this` receiver are
        // production-method candidates.
        let receiver = n.child_by_field_name("object");
        let receiver_ok = match receiver {
            None => true,
            Some(obj) => obj.kind() == "this",
        };
        if !receiver_ok {
            continue;
        }
        let Some(name_node) = n.child_by_field_name("name") else {
            continue;
        };
        if let Ok(text) = name_node.utf8_text(bytes) {
            out.push(text.to_string());
        }
    }
    out
}

fn reindent_method_to_block(source: &str, start: usize, end: usize) -> String {
    let raw = &source[leading_trivia(start, source)..trailing_newline(end, source)];
    let trimmed = raw.trim_end_matches('\n');
    trimmed.to_string()
}

fn leading_trivia(start: usize, source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
        } else if b == b'\n' {
            // include the newline preceding the method
            cursor -= 1;
            break;
        } else {
            break;
        }
    }
    cursor
}

fn trailing_newline(end: usize, source: &str) -> usize {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut cursor = end;
    while cursor < len {
        let b = bytes[cursor];
        if b == b' ' || b == b'\t' {
            cursor += 1;
            continue;
        }
        if b == b'\n' {
            cursor += 1;
            break;
        }
        break;
    }
    cursor
}


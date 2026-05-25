//! `extract_java_test_slice` — split a source test class around an
//! `extract_java_class` production move.
//!
//! Three outcomes per @Test method, by which methods it invokes:
//!
//! - **only-moved** — migrated to the target test class.
//! - **only-kept** — left alone.
//! - **mixed** (calls both moved AND kept methods) — when the source
//!   test class uses MockitoExtension AND the operator supplies the
//!   target production type, the planner synthesizes `@Mock <Target>
//!   mockTarget;` on the source test class and rewrites moved-method
//!   call sites to use the mock. Otherwise refuse with operator
//!   guidance.
//!
//! ## Inputs
//!
//! - `source` (required) — source test class file
//! - `target` (required) — target test class file (must not exist)
//! - `project_dir` (required)
//! - `item_names` (required) — list of moved production method names
//! - `module_name` (optional) — target test class name (default from
//!   target filename)
//! - `impl_name` (optional) — source test class name (when source has
//!   multiple top-level classes)
//! - `delegate_type` (optional) — target production type (e.g.
//!   `AuthorizationService`). When supplied AND source uses Mockito,
//!   mixed-coverage tests get `@Mock` + call rewrites instead of being
//!   refused.
//!
//! ## Refusals
//!
//! - `error.no_test_methods_in_source`
//! - `error.no_tests_to_migrate_or_mock` — no tests touch moved methods
//! - `error.mixed_coverage_without_mockito` — mixed tests exist, but
//!   the source test class isn't Mockito-driven (no
//!   `@ExtendWith(MockitoExtension.class)`, no existing `@Mock` field).
//!   Operator should: (a) split the mixed tests manually via
//!   `extract_java_methods`, (b) add Mockito to the test class, or
//!   (c) supply `delegate_type` to opt into auto-mock generation.
//! - `error.mixed_coverage_without_delegate_type` — class is Mockito-
//!   driven but operator didn't supply `delegate_type`. Provide it.
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
    let mut mixed_tests: Vec<&TestMethod> = Vec::new();
    for test in &tests {
        let invocations = collect_bare_invocations(test.node, &parsed.source);
        let touches_moved = invocations.iter().any(|n| moved_names.contains(n));
        let touches_kept = invocations.iter().any(|n| !moved_names.contains(n));
        match (touches_moved, touches_kept) {
            (true, false) => to_move.push(test),
            (true, true) => mixed_tests.push(test),
            _ => {}
        }
    }
    let mixed_names: Vec<String> = mixed_tests.iter().map(|t| t.name.clone()).collect();
    // v2: when mixed tests exist, generate Mockito boilerplate
    // (`@Mock <Target>` + call rewrites) if the source test class
    // uses Mockito and operator supplied delegate_type.
    let delegate_type = p
        .delegate_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let class_is_mockito = is_mockito_driven(class_node, &parsed.source);
    if !mixed_tests.is_empty() {
        if !class_is_mockito {
            bail!(
                "error.mixed_coverage_without_mockito: test method(s) {} exercise both moved \
                 and kept production methods, but the source test class is not Mockito-driven \
                 (no @ExtendWith(MockitoExtension.class) and no @Mock fields). Operator workflow: \
                 (a) split the mixed tests via `extract_java_methods` to single-coverage tests, \
                 OR (b) add MockitoExtension to the test class, then re-run with `delegate_type`.",
                mixed_names.join(", ")
            );
        }
        if delegate_type.is_none() {
            bail!(
                "error.mixed_coverage_without_delegate_type: test method(s) {} exercise both \
                 moved and kept production methods, and the source test class is Mockito-driven. \
                 Supply `delegate_type` (the target production type, e.g. \
                 `\"AuthorizationService\"`) to enable @Mock generation for moved-method calls.",
                mixed_names.join(", ")
            );
        }
    }
    if to_move.is_empty() && mixed_tests.is_empty() {
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
    let target_text = format!("{prelude}public class {target_class_name} {{\n{body_joined}\n}}\n");

    // Build the source-side edits.
    let mut source_edits: Vec<TextEdit> = to_move
        .iter()
        .map(|t| TextEdit {
            byte_start: leading_trivia(t.byte_start, &parsed.source),
            byte_end: trailing_newline(t.byte_end, &parsed.source),
            replacement: String::new(),
        })
        .collect();

    // v2: Mockito boilerplate for mixed-coverage tests.
    let mut mock_field_name = String::new();
    if !mixed_tests.is_empty() {
        let target_type = delegate_type.as_deref().unwrap();
        let (field_name, mock_field_edits) =
            ensure_mock_field(class_node, &parsed.source, target_type);
        mock_field_name = field_name;
        source_edits.extend(mock_field_edits);

        // Rewrite each moved-method invocation inside mixed tests to
        // be qualified with the mock field name.
        for test in &mixed_tests {
            let rewrites = find_moved_invocation_rewrites(
                test.node,
                &parsed.source,
                &moved_names,
                &mock_field_name,
            );
            source_edits.extend(rewrites);
        }
    }

    source_edits = crate::refactor::java::di_plumbing::dedupe_insertion_edits(source_edits);
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;

    let moved_names_list = to_move
        .iter()
        .map(|t| t.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let mut file_edits = vec![FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
        new_text: None,
    }];
    if !to_move.is_empty() {
        file_edits.push(FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(b""),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: 0,
                replacement: String::new(),
            }],
            new_text: Some(target_text),
        });
    }
    let mut validations = parse_validation_step_for_path(&source_path);
    if !to_move.is_empty() {
        validations.extend(parse_validation_step_for_path(&target_path));
    }

    let mut leftovers = Vec::new();
    if !to_move.is_empty() {
        leftovers.push(format!("migrated_tests={moved_names_list}"));
    }
    if !mixed_tests.is_empty() {
        leftovers.push(format!(
            "mocked_tests={} (using {})",
            mixed_names.join(", "),
            mock_field_name
        ));
    }

    let plan = RefactorPlan {
        title: format!(
            "extract_java_test_slice: {} migrated, {} mocked in {}",
            to_move.len(),
            mixed_tests.len(),
            path_string(&source_path)
        ),
        kind: "extract_java_test_slice".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers,
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: Vec::new(),
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

/// Returns true if the source test class uses Mockito (has
/// `@ExtendWith(MockitoExtension.class)` annotation OR at least one
/// `@Mock`-annotated field).
fn is_mockito_driven(class_node: Node<'_>, source: &str) -> bool {
    // Check @ExtendWith on the class itself.
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for mod_child in child.children(&mut mc) {
            let mk = mod_child.kind();
            if !(mk == "marker_annotation" || mk == "annotation") {
                continue;
            }
            let name = mod_child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .unwrap_or("");
            if name == "ExtendWith" {
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    if text.contains("MockitoExtension") {
                        return true;
                    }
                }
            }
            if name == "RunWith" {
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    if text.contains("MockitoJUnitRunner") {
                        return true;
                    }
                }
            }
        }
    }
    // Check for any @Mock field.
    let Some(body) = class_node.child_by_field_name("body") else {
        return false;
    };
    let mut bc = body.walk();
    for member in body.named_children(&mut bc) {
        if member.kind() != "field_declaration" {
            continue;
        }
        if has_annotation_in(member, source, "Mock") {
            return true;
        }
    }
    false
}

/// Find an existing `@Mock <Target>` field, or generate an edit that
/// adds one. Also generates `import org.mockito.Mock;` if needed.
fn ensure_mock_field(
    class_node: Node<'_>,
    source: &str,
    target_type: &str,
) -> (String, Vec<TextEdit>) {
    let bytes = source.as_bytes();
    if let Some(body) = class_node.child_by_field_name("body") {
        let mut bc = body.walk();
        for member in body.named_children(&mut bc) {
            if member.kind() != "field_declaration" {
                continue;
            }
            if !has_annotation_in(member, source, "Mock") {
                continue;
            }
            let Some(type_node) = member.child_by_field_name("type") else {
                continue;
            };
            let field_type = type_node.utf8_text(bytes).unwrap_or("").trim();
            if field_type != target_type {
                continue;
            }
            let mut mc = member.walk();
            for decl in member.named_children(&mut mc) {
                if decl.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(name_node) = decl.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(bytes) {
                        return (name.to_string(), Vec::new());
                    }
                }
            }
        }
    }
    // Generate a new @Mock field.
    let field_name = derive_mock_field_name(target_type);
    let decl_text = format!("\n    @Mock\n    {target_type} {field_name};\n");
    let insert_byte = body_insert_byte_after_last_field(class_node);
    let mut edits = vec![TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: decl_text,
    }];
    if !source.contains("import org.mockito.Mock;") {
        if let Some(edit) = synth_import_edit(source, "org.mockito.Mock") {
            edits.push(edit);
        }
    }
    (field_name, edits)
}

fn derive_mock_field_name(target_type: &str) -> String {
    let bare = target_type
        .split('<')
        .next()
        .unwrap_or(target_type)
        .rsplit('.')
        .next()
        .unwrap_or(target_type);
    let mut chars = bare.chars();
    let lower = match chars.next() {
        Some(c) => c.to_lowercase().to_string(),
        None => String::new(),
    };
    let rest: String = chars.collect();
    format!("mock{}{}", lower.to_uppercase(), rest)
}

fn body_insert_byte_after_last_field(class_node: Node<'_>) -> usize {
    let body = match class_node.child_by_field_name("body") {
        Some(b) => b,
        None => return 0,
    };
    let mut last_field_end: Option<usize> = None;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() == "field_declaration" {
            last_field_end = Some(member.end_byte());
        }
    }
    last_field_end.unwrap_or_else(|| body.start_byte() + 1)
}

fn synth_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let imports = extract_existing_imports(source);
    let insert_byte = if let Some((_, end)) = imports.last() {
        *end
    } else if let Some(pkg_end) = find_package_decl_end(source) {
        pkg_end
    } else {
        return Some(TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: format!("import {fqcn};\n"),
        });
    };
    Some(TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: format!("\nimport {fqcn};"),
    })
}

fn extract_existing_imports(source: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in source.lines() {
        let line_start = start;
        let line_end = start + line.len();
        let trimmed = line.trim();
        if trimmed.starts_with("import ") && trimmed.ends_with(';') {
            out.push((line_start, line_end));
        }
        start = line_end + 1;
    }
    out
}

fn find_package_decl_end(source: &str) -> Option<usize> {
    let mut start = 0;
    for line in source.lines() {
        let line_end = start + line.len();
        let trimmed = line.trim();
        if trimmed.starts_with("package ") && trimmed.ends_with(';') {
            return Some(line_end);
        }
        if !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with("/*") {
            return None;
        }
        start = line_end + 1;
    }
    None
}

/// For each bare method_invocation inside `test_node` whose name is in
/// `moved_names`, generate an edit that inserts `<mock_field_name>.`
/// before the method-name token.
fn find_moved_invocation_rewrites(
    test_node: Node<'_>,
    source: &str,
    moved_names: &HashSet<String>,
    mock_field_name: &str,
) -> Vec<TextEdit> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![test_node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for child in n.named_children(&mut c) {
            stack.push(child);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        if let Some(obj) = n.child_by_field_name("object") {
            // Has explicit receiver. Only rewrite when receiver is `this`.
            if obj.kind() != "this" {
                continue;
            }
            // Replace the entire `this.` prefix with `<mock>.`.
            let Some(name_node) = n.child_by_field_name("name") else {
                continue;
            };
            let Ok(name) = name_node.utf8_text(bytes) else {
                continue;
            };
            if !moved_names.contains(name) {
                continue;
            }
            // Edit: replace from obj.start to name.start with "<mock>.".
            out.push(TextEdit {
                byte_start: obj.start_byte(),
                byte_end: name_node.start_byte(),
                replacement: format!("{mock_field_name}."),
            });
        } else {
            // Receiverless call. Insert `<mock>.` before the name.
            let Some(name_node) = n.child_by_field_name("name") else {
                continue;
            };
            let Ok(name) = name_node.utf8_text(bytes) else {
                continue;
            };
            if !moved_names.contains(name) {
                continue;
            }
            out.push(TextEdit {
                byte_start: name_node.start_byte(),
                byte_end: name_node.start_byte(),
                replacement: format!("{mock_field_name}."),
            });
        }
    }
    out
}

//! Production-side conversions for the static-pattern triad
//! (note-7c819189, note-e5439c0a, note-7d4f0001). Two plan kinds:
//!
//! - `singletonify_java_holder` — convert a class with
//!   `public static final <T> NAME = <expr>;` declarations into a
//!   `@Singleton` class with private final fields, an `@Inject`
//!   constructor that accepts each declared type, and the original
//!   field initializers dropped (the DI container is now responsible
//!   for injection). Pair with `replace_java_static_reference`
//!   (caller-side) to fully unwind a static-holder pattern.
//!
//! - `singletonify_java_util` — convert a `*Util`-style class with
//!   `public static <ret> method(...)` declarations into a
//!   `@Singleton` class with instance methods. Includes a pure-method
//!   classifier that refuses to convert genuinely-pure utility methods
//!   (those that read no static state and call no other non-pure
//!   static methods). The operator must select which subset of methods
//!   to convert by name via `item_names`.
//!
//! ## Pure-method classifier (singletonify_java_util)
//!
//! A method is considered PURE iff:
//! - it does not read or write any static field on the same class,
//! - it does not call any other static method on the same class
//!   (transitively pure callees would be safe but v1 doesn't compute
//!   the transitive closure),
//! - it does not have side effects detectable by AST (no
//!   `System.out`/`Logger`/etc. — these are detected by the heuristic:
//!   no method_invocation whose qualifier matches `System`, `Logger`,
//!   `Files`, etc.). v1's pure check is intentionally conservative;
//!   when ambiguous the planner classifies as IMPURE (=convertible).
//!
//! Refusing pure-method conversion is a feature: pure formatters
//! (`Integer.parseInt`-style) shouldn't be threaded through DI just
//! because they live on a Util class.

use super::*;
use std::collections::{HashMap, HashSet};

// ============================================================================
// singletonify_java_holder
// ============================================================================

pub(crate) fn plan_singletonify_java_holder(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("singletonify_java_holder only supports java files");
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
    if class_node.kind() == "interface_declaration" {
        bail!("singletonify_java_holder does not operate on interfaces");
    }

    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("class has no name"))?;

    // Collect `public static final <T> NAME = <expr>;` declarations.
    let holder_fields = collect_public_static_final_fields(class_node, &parsed.source);
    if holder_fields.is_empty() {
        bail!(
            "no `public static final <T> NAME = <expr>;` declarations found on class `{class_name}` \
             — nothing to singletonify"
        );
    }

    // Apply name_filter if operator restricted by item_names.
    let name_filter: Option<HashSet<String>> = p
        .item_names
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| v.iter().cloned().collect());
    let selected: Vec<&HolderField> = holder_fields
        .iter()
        .filter(|f| {
            name_filter
                .as_ref()
                .map(|s| s.contains(&f.name))
                .unwrap_or(true)
        })
        .collect();
    if selected.is_empty() {
        bail!(
            "operator-supplied item_names didn't match any public-static-final fields on \
             class `{class_name}`"
        );
    }

    // Build per-field rewrite text: `private final <T> <name>;` (lower-camel from CONSTANT_CASE).
    let mut edits = Vec::new();
    let mut ctor_params: Vec<(String, String)> = Vec::new(); // (type, name)
    for f in &selected {
        let new_field_name = to_camel_case_from_const(&f.name);
        let replacement = format!("private final {} {};", f.type_text, new_field_name);
        edits.push(TextEdit {
            byte_start: f.byte_start,
            byte_end: f.byte_end,
            replacement,
        });
        ctor_params.push((f.type_text.clone(), new_field_name));
    }

    // Add @Singleton annotation to the class declaration if not present.
    let inject_ns = crate::refactor::java::di_plumbing::InjectNamespace::detect(&parsed.source);
    let singleton_fqcn = match inject_ns {
        crate::refactor::java::di_plumbing::InjectNamespace::Javax => "javax.inject.Singleton",
        _ => "jakarta.inject.Singleton",
    };
    let inject_fqcn = inject_ns.inject_fqcn();
    let mut imports_needed: Vec<String> = vec![singleton_fqcn.to_string(), inject_fqcn.to_string()];

    // Insert @Singleton annotation before the class keyword.
    if !class_has_annotation(class_node, &parsed.source, "Singleton") {
        let insert_byte = class_annotation_insert_byte(class_node);
        edits.push(TextEdit {
            byte_start: insert_byte,
            byte_end: insert_byte,
            replacement: "@Singleton\n".to_string(),
        });
    } else {
        // Already annotated — no Singleton import needed (it's there).
        imports_needed.retain(|s| !s.ends_with(".Singleton"));
    }

    // Generate constructor.
    let class_body = class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("class has no body"))?;
    let body_open_brace = class_body.start_byte() + 1; // after `{`
    let ctor_params_text = ctor_params
        .iter()
        .map(|(t, n)| format!("{t} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let ctor_assigns = ctor_params
        .iter()
        .map(|(_, n)| format!("        this.{n} = {n};"))
        .collect::<Vec<_>>()
        .join("\n");
    let ctor_text = format!(
        "\n\n    @Inject\n    public {class_name}({ctor_params_text}) {{\n{ctor_assigns}\n    }}"
    );
    edits.push(TextEdit {
        byte_start: body_open_brace,
        byte_end: body_open_brace,
        replacement: ctor_text,
    });

    // Add required imports.
    for fqcn in &imports_needed {
        if let Some(edit) = synth_import_edit(&parsed.source, fqcn) {
            edits.push(edit);
        }
    }

    edits = crate::refactor::java::di_plumbing::dedupe_insertion_edits(edits);
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "singletonify class `{class_name}` — convert {} holder field(s) to @Inject constructor",
            selected.len()
        ),
        kind: "singletonify_java_holder".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: vec![format!(
            "converted_fields={:?}",
            selected.iter().map(|f| &f.name).collect::<Vec<_>>()
        )],
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

#[derive(Debug, Clone)]
struct HolderField {
    name: String,
    type_text: String,
    byte_start: usize,
    byte_end: usize,
}

fn collect_public_static_final_fields(class_node: Node<'_>, source: &str) -> Vec<HolderField> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let mut has_public = false;
        let mut has_static = false;
        let mut has_final = false;
        let mut mc = member.walk();
        for c in member.children(&mut mc) {
            if c.kind() == "modifiers" {
                let mut mod_cursor = c.walk();
                for mod_child in c.children(&mut mod_cursor) {
                    match mod_child.kind() {
                        "public" => has_public = true,
                        "static" => has_static = true,
                        "final" => has_final = true,
                        _ => {}
                    }
                }
            }
        }
        if !(has_public && has_static && has_final) {
            continue;
        }
        let Some(type_node) = member.child_by_field_name("type") else {
            continue;
        };
        let type_text = type_node.utf8_text(bytes).unwrap_or("").trim().to_string();
        // Skip serialVersionUID.
        let mut dc = member.walk();
        for decl in member.named_children(&mut dc) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = decl.child_by_field_name("name") else {
                continue;
            };
            let Ok(name) = name_node.utf8_text(bytes) else {
                continue;
            };
            if name == "serialVersionUID" {
                continue;
            }
            out.push(HolderField {
                name: name.to_string(),
                type_text: type_text.clone(),
                byte_start: leading_trivia_start_of_node(source, member),
                byte_end: trailing_newline_after(source, member.end_byte()),
            });
            break; // one declarator per holder field for v1
        }
    }
    out
}

/// `SITE_ADMIN` → `siteAdmin`, `PLANT_REPO` → `plantRepo`.
fn to_camel_case_from_const(name: &str) -> String {
    let parts: Vec<&str> = name.split('_').collect();
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(&part.to_lowercase());
        } else if let Some(first) = part.chars().next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(&part[first.len_utf8()..].to_lowercase());
        }
    }
    out
}

fn class_has_annotation(class_node: Node<'_>, source: &str, annotation: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for m in child.children(&mut mc) {
            if matches!(m.kind(), "marker_annotation" | "annotation") {
                let name = m
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(bytes).ok())
                    .unwrap_or("");
                if name == annotation {
                    return true;
                }
            }
        }
    }
    false
}

fn class_annotation_insert_byte(class_node: Node<'_>) -> usize {
    // Find the "class" keyword child to insert before. The modifiers
    // node is the first child if present; otherwise the class
    // declaration starts at the keyword.
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() == "class" || child.kind() == "interface" {
            return child.start_byte();
        }
    }
    class_node.start_byte()
}

fn leading_trivia_start_of_node(source: &str, node: Node<'_>) -> usize {
    // Consume leading whitespace on the current line but NOT the
    // preceding newline (that newline belongs to the previous member's
    // trailing range; consuming it here would cause edit overlap).
    let bytes = source.as_bytes();
    let mut cursor = node.start_byte();
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
            continue;
        }
        break;
    }
    cursor
}

fn trailing_newline_after(source: &str, end: usize) -> usize {
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

fn synth_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let needle = format!("import {fqcn};");
    if source.contains(&needle) {
        return None;
    }
    let imports = extract_imports(source);
    let insert_byte = if let Some((_, end)) = imports.last() {
        *end
    } else if let Some(pkg_end) = find_package_decl_end(source) {
        pkg_end
    } else {
        0
    };
    Some(TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: format!("\nimport {fqcn};"),
    })
}

fn extract_imports(source: &str) -> Vec<(usize, usize)> {
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

// ============================================================================
// singletonify_java_util
// ============================================================================

pub(crate) fn plan_singletonify_java_util(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("singletonify_java_util only supports java files");
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
    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("class has no name"))?;

    let item_names: HashSet<String> = p
        .item_names
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "item_names (list of public static method names to convert to instance) is required"
            )
        })?
        .iter()
        .cloned()
        .collect();

    let methods = collect_public_static_methods(class_node, &parsed.source);
    if methods.is_empty() {
        bail!("no public static methods found on class `{class_name}`");
    }
    let static_field_names = collect_class_static_field_names(class_node, &parsed.source);
    let class_method_names_static: HashSet<String> =
        methods.iter().map(|m| m.name.clone()).collect();

    let mut to_convert: Vec<&UtilMethod> = Vec::new();
    let mut refused_pure: Vec<String> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    let selected_names: Vec<String> = item_names.iter().cloned().collect();
    for sel in &selected_names {
        let Some(m) = methods.iter().find(|m| &m.name == sel) else {
            not_found.push(sel.clone());
            continue;
        };
        if is_pure_method(
            m.node,
            &parsed.source,
            &static_field_names,
            &class_method_names_static,
        ) {
            refused_pure.push(sel.clone());
        } else {
            to_convert.push(m);
        }
    }
    if !not_found.is_empty() {
        bail!(
            "method(s) {} not found as public static on class `{class_name}`",
            not_found.join(", ")
        );
    }
    if !refused_pure.is_empty() {
        bail!(
            "error.pure_methods_refused({}): the following methods read no static state, call no \
             other non-pure static methods, and have no observable side effects in the AST — they \
             are genuinely pure utility functions and should stay static. Either pick a different \
             set via item_names, or refactor those callers to use their pure-static form.",
            refused_pure.join(", ")
        );
    }
    if to_convert.is_empty() {
        bail!("nothing to convert on class `{class_name}`");
    }

    // Remove `static` from each converted method's modifiers.
    let mut edits: Vec<TextEdit> = Vec::new();
    for m in &to_convert {
        if let Some((s, e)) = m.static_keyword_range {
            // Delete the `static ` token (including trailing space).
            let mut delete_end = e;
            let bytes = parsed.source.as_bytes();
            while delete_end < bytes.len()
                && (bytes[delete_end] == b' ' || bytes[delete_end] == b'\t')
            {
                delete_end += 1;
            }
            edits.push(TextEdit {
                byte_start: s,
                byte_end: delete_end,
                replacement: String::new(),
            });
        }
    }

    // Add @Singleton + @Inject (no-arg constructor for now; this is the
    // minimal change — operator can add fields later).
    let inject_ns = crate::refactor::java::di_plumbing::InjectNamespace::detect(&parsed.source);
    let singleton_fqcn = match inject_ns {
        crate::refactor::java::di_plumbing::InjectNamespace::Javax => "javax.inject.Singleton",
        _ => "jakarta.inject.Singleton",
    };
    if !class_has_annotation(class_node, &parsed.source, "Singleton") {
        let insert_byte = class_annotation_insert_byte(class_node);
        edits.push(TextEdit {
            byte_start: insert_byte,
            byte_end: insert_byte,
            replacement: "@Singleton\n".to_string(),
        });
        if let Some(edit) = synth_import_edit(&parsed.source, singleton_fqcn) {
            edits.push(edit);
        }
    }

    edits = crate::refactor::java::di_plumbing::dedupe_insertion_edits(edits);
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "singletonify util class `{class_name}` — {} method(s) → instance",
            to_convert.len()
        ),
        kind: "singletonify_java_util".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: vec![format!(
            "converted_methods={:?}",
            to_convert.iter().map(|m| &m.name).collect::<Vec<_>>()
        )],
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

#[derive(Debug, Clone)]
struct UtilMethod<'a> {
    node: Node<'a>,
    name: String,
    /// Byte range of the `static` modifier keyword, if present.
    static_keyword_range: Option<(usize, usize)>,
}

fn collect_public_static_methods<'a>(class_node: Node<'a>, source: &str) -> Vec<UtilMethod<'a>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        let (has_public, has_static, static_range) = scan_method_modifiers(member);
        if !(has_public && has_static) {
            continue;
        }
        let Some(name_node) = member.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(bytes) else {
            continue;
        };
        out.push(UtilMethod {
            node: member,
            name: name.to_string(),
            static_keyword_range: static_range,
        });
    }
    out
}

fn scan_method_modifiers(method: Node<'_>) -> (bool, bool, Option<(usize, usize)>) {
    let mut has_public = false;
    let mut has_static = false;
    let mut static_range: Option<(usize, usize)> = None;
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let mut mc = child.walk();
            for m in child.children(&mut mc) {
                match m.kind() {
                    "public" => has_public = true,
                    "static" => {
                        has_static = true;
                        static_range = Some((m.start_byte(), m.end_byte()));
                    }
                    _ => {}
                }
            }
        }
    }
    (has_public, has_static, static_range)
}

fn collect_class_static_field_names(class_node: Node<'_>, source: &str) -> HashSet<String> {
    let bytes = source.as_bytes();
    let mut out = HashSet::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let mut has_static = false;
        let mut mc = member.walk();
        for c in member.children(&mut mc) {
            if c.kind() == "modifiers" {
                let mut mod_cursor = c.walk();
                for m in c.children(&mut mod_cursor) {
                    if m.kind() == "static" {
                        has_static = true;
                    }
                }
            }
        }
        if !has_static {
            continue;
        }
        let mut dc = member.walk();
        for decl in member.named_children(&mut dc) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = decl.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(bytes) {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Conservative purity check: returns true iff the method body does
/// NOT read/write any static field on the same class AND does NOT call
/// any static method on the same class.
fn is_pure_method(
    method: Node<'_>,
    source: &str,
    static_fields: &HashSet<String>,
    static_methods: &HashSet<String>,
) -> bool {
    let bytes = source.as_bytes();
    let Some(body) = method.child_by_field_name("body") else {
        return true; // abstract / no body — treat as pure (won't be converted)
    };
    let mut stack = vec![body];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        match n.kind() {
            "identifier" => {
                if let Ok(text) = n.utf8_text(bytes) {
                    if static_fields.contains(text) {
                        return false;
                    }
                }
            }
            "method_invocation" => {
                // Receiverless call to a known same-class static method?
                if n.child_by_field_name("object").is_none() {
                    if let Some(name_node) = n.child_by_field_name("name") {
                        if let Ok(text) = name_node.utf8_text(bytes) {
                            if static_methods.contains(text) {
                                return false;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    true
}

#[allow(dead_code)]
fn _placeholder_for_unused_map() -> HashMap<String, String> {
    HashMap::new()
}

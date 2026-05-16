mod cross_file;
use super::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone)]
pub(crate) struct JavaMethod {
    #[allow(dead_code)]
    parent_name: String,
    #[allow(dead_code)]
    parent_byte_start: usize,
    item: SyntaxItem,
}

#[derive(Debug, Clone)]
pub(crate) struct JavaNestedClass {
    #[allow(dead_code)]
    parent_name: String,
    #[allow(dead_code)]
    parent_byte_start: usize,
    item: SyntaxItem,
}

#[derive(Debug, Clone)]
struct JavaField {
    name: String,
    type_name: String,
    item: SyntaxItem,
    is_final: bool,
}

pub(crate) fn java_methods(parsed: &ParsedSource) -> Vec<JavaMethod> {
    let mut methods = Vec::new();
    let root = parsed.tree.root_node();
    walk_java_methods(parsed, root, "(root)", 0, &mut methods);
    methods
}

pub(crate) fn java_status_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let mut items = generic_top_level_items(parsed);
    items.extend(java_methods(parsed).into_iter().map(|method| method.item));
    items.extend(
        java_nested_classes(parsed)
            .into_iter()
            .map(|class| class.item),
    );
    items
}

fn walk_java_methods(
    parsed: &ParsedSource,
    node: Node<'_>,
    parent_name: &str,
    parent_byte_start: usize,
    methods: &mut Vec<JavaMethod>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration"
            || kind == "interface_declaration"
            || kind == "record_declaration"
            || kind == "enum_declaration"
        {
            let name = item_name(child, &parsed.source, parsed.language)
                .unwrap_or_else(|| "(unnamed)".to_string());
            if let Some(body) = child.child_by_field_name("body") {
                walk_java_methods(parsed, body, &name, child.start_byte(), methods);
            } else if let Some(_body) = child.child_by_field_name("interfaces") {
                walk_java_methods(parsed, child, &name, child.start_byte(), methods);
            } else {
                walk_java_methods(parsed, child, &name, child.start_byte(), methods);
            }
        } else if kind == "class_body" || kind == "enum_body" || kind == "record_body" {
            walk_java_methods(parsed, child, parent_name, parent_byte_start, methods);
        } else if kind == "method_declaration" || kind == "constructor_declaration" {
            methods.push(JavaMethod {
                parent_name: parent_name.to_string(),
                parent_byte_start,
                item: syntax_item_with_kind(parsed, child, kind),
            });
        } else {
            walk_java_methods(parsed, child, parent_name, parent_byte_start, methods);
        }
    }
}

pub(crate) fn java_nested_classes(parsed: &ParsedSource) -> Vec<JavaNestedClass> {
    let mut classes = Vec::new();
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration"
            || kind == "interface_declaration"
            || kind == "record_declaration"
            || kind == "enum_declaration"
        {
            let name = item_name(child, &parsed.source, parsed.language)
                .unwrap_or_else(|| "(unnamed)".to_string());
            walk_java_nested_classes(parsed, child, &name, child.start_byte(), &mut classes);
        }
    }
    classes
}

fn walk_java_nested_classes(
    parsed: &ParsedSource,
    node: Node<'_>,
    parent_name: &str,
    parent_byte_start: usize,
    classes: &mut Vec<JavaNestedClass>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();
        if kind == "class_declaration"
            || kind == "interface_declaration"
            || kind == "record_declaration"
            || kind == "enum_declaration"
        {
            classes.push(JavaNestedClass {
                parent_name: parent_name.to_string(),
                parent_byte_start,
                item: syntax_item_with_kind(parsed, child, kind),
            });
            let name = item_name(child, &parsed.source, parsed.language)
                .unwrap_or_else(|| "(unnamed)".to_string());
            walk_java_nested_classes(parsed, child, &name, child.start_byte(), classes);
        } else if kind == "class_body" || kind == "enum_body" || kind == "record_body" {
            walk_java_nested_classes(parsed, child, parent_name, parent_byte_start, classes);
        }
    }
}

fn find_first_class_declaration(root: Node<'_>) -> Option<Node<'_>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_declaration" || node.kind() == "record_declaration" {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn find_class_declaration_by_name<'a>(
    parsed: &'a ParsedSource,
    class_name: &str,
) -> Option<Node<'a>> {
    let root = parsed.tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if kind == "class_declaration" || kind == "record_declaration" {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                    if name == class_name {
                        return Some(node);
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn validate_java_type_identifier(name: &str, field: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        bail!("{field} must be a valid Java type identifier, got `{name}`");
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        bail!("{field} must be a valid Java type identifier, got `{name}`");
    }
    Ok(())
}

fn java_target_type_name(p: &RefactorPlanParams, target_path: &Path) -> Result<String> {
    let name = p
        .module_name
        .clone()
        .or_else(|| {
            target_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("module_name is required when target file has no stem"))?;
    validate_java_type_identifier(&name, "module_name")?;
    Ok(name)
}

fn java_default_target_prelude(
    p: &RefactorPlanParams,
    source: &str,
    resolved_package: Option<&str>,
) -> String {
    if let Some(prelude) = p.target_prelude.as_deref() {
        let trimmed = prelude.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        return format!("{trimmed}\n\n");
    }
    let pkg = resolved_package
        .map(str::to_string)
        .or_else(|| extract_java_package(source));
    let imports = extract_java_imports(source);
    match (pkg, imports.is_empty()) {
        (Some(pkg), true) => format!("package {pkg};\n\n"),
        (Some(pkg), false) => format!("package {pkg};\n\n{}\n\n", imports.join("\n")),
        (None, true) => String::new(),
        (None, false) => format!("{}\n\n", imports.join("\n")),
    }
}

fn java_class_wrapper(class_name: &str, prelude: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str(prelude);
    out.push_str(&format!("public class {class_name} {{\n"));
    if !body.trim().is_empty() {
        out.push_str(body.trim_matches('\n'));
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// Inject `implements I1, I2` into the `public class <Name>` declaration of a
/// generated target file produced by `java_class_wrapper`. Idempotent: if the
/// declaration already lists implements, the new names are appended.
fn java_inject_implements(target_text: &str, class_name: &str, interfaces: &[String]) -> String {
    if interfaces.is_empty() {
        return target_text.to_string();
    }
    let needle = format!("public class {class_name}");
    let Some(decl_start) = target_text.find(&needle) else {
        return target_text.to_string();
    };
    // Locate the `{` that opens the class body. Anything between needle's end
    // and `{` is the (possibly empty) extends/implements clause area.
    let after_needle = decl_start + needle.len();
    let Some(brace_rel) = target_text[after_needle..].find('{') else {
        return target_text.to_string();
    };
    let brace_at = after_needle + brace_rel;
    let between = &target_text[after_needle..brace_at];
    let trimmed = between.trim();
    let new_clause = if let Some(rest) = trimmed.strip_prefix("implements") {
        // Already has implements — append.
        let existing = rest.trim_end().to_string();
        let joined = interfaces.join(", ");
        format!(" implements {existing}, {joined} ")
    } else if let Some(rest) = trimmed.strip_prefix("extends") {
        // extends X — preserve and add implements.
        let joined = interfaces.join(", ");
        format!(" extends {} implements {joined} ", rest.trim())
    } else if trimmed.is_empty() {
        let joined = interfaces.join(", ");
        format!(" implements {joined} ")
    } else {
        // Unknown content — do not mangle.
        return target_text.to_string();
    };
    let mut out = String::with_capacity(target_text.len() + new_clause.len());
    out.push_str(&target_text[..after_needle]);
    out.push_str(&new_clause);
    out.push_str(&target_text[brace_at..]);
    out
}

/// Insert an `import <fqcn>;` line into the import block of a Java target file.
/// If an identical import already exists, returns the original text unchanged.
fn java_inject_import(target_text: &str, fqcn: &str) -> String {
    let import_line = format!("import {fqcn};");
    if target_text.lines().any(|line| line.trim() == import_line) {
        return target_text.to_string();
    }
    // Same dedupe shape as java_source_import_edit — skip when a
    // different FQCN with the same simple name is already imported, or
    // when an existing wildcard import covers the SAME package as the
    // new import (in which case it's redundant).
    let desired_simple = fqcn.rsplit('.').next().unwrap_or("");
    let desired_pkg = java_fqcn_package(fqcn);
    if !desired_simple.is_empty() {
        for line in target_text.lines() {
            if let Some(existing) = java_import_simple_name(line) {
                if existing == desired_simple {
                    return target_text.to_string();
                }
            }
            if let Some(wildcard_pkg) = java_import_wildcard_package(line) {
                if Some(wildcard_pkg.as_str()) == desired_pkg {
                    return target_text.to_string();
                }
            }
        }
    }
    // Place after the last existing import; otherwise after the package line;
    // otherwise at the top.
    let mut last_import_end: Option<usize> = None;
    let mut package_end: Option<usize> = None;
    let mut offset = 0usize;
    for line in target_text.split_inclusive('\n') {
        let trimmed = line.trim();
        let line_end = offset + line.len();
        if trimmed.starts_with("package ") {
            package_end = Some(line_end);
        }
        if trimmed.starts_with("import ") {
            last_import_end = Some(line_end);
        }
        offset = line_end;
    }
    let (insert_at, prefix, suffix) = if let Some(end) = last_import_end {
        (end, "", "\n")
    } else if let Some(end) = package_end {
        (end, "\n", "\n")
    } else {
        (0, "", "\n\n")
    };
    let mut out = String::with_capacity(target_text.len() + import_line.len() + 2);
    out.push_str(&target_text[..insert_at]);
    out.push_str(prefix);
    out.push_str(&import_line);
    out.push_str(suffix);
    out.push_str(&target_text[insert_at..]);
    out
}

/// Insert FIXME comment lines above each unqualified call site of `method` in
/// `target_text`. The comment is indented to match the call site's column.
/// Skips matches that are preceded by `.` or `::` (i.e. qualified by a
/// receiver) or by an identifier character (e.g. `myMethod` is not a match
/// for `method`). Returns `(new_text, count_inserted)`.
fn java_insert_fixme_above_calls(
    target_text: &str,
    method: &str,
    fixme_lines: &[String],
) -> (String, usize) {
    if fixme_lines.is_empty() {
        return (target_text.to_string(), 0);
    }
    let mut out = String::with_capacity(target_text.len() + fixme_lines.len() * 80);
    let bytes = target_text.as_bytes();
    let needle = method.as_bytes();
    let mut cursor = 0usize;
    let mut inserted = 0usize;
    let mut copy_from = 0usize;
    while cursor + needle.len() <= bytes.len() {
        let Some(rel) = target_text[cursor..].find(method) else {
            break;
        };
        let pos = cursor + rel;
        let after = pos + needle.len();
        // Must be unqualified: the byte before, if any, is not `.`, `:` or an
        // identifier-continuation char.
        let prev_ok = if pos == 0 {
            true
        } else {
            let prev = bytes[pos - 1] as char;
            !(prev == '.'
                || prev == ':'
                || prev == '_'
                || prev == '$'
                || prev.is_ascii_alphanumeric())
        };
        // Must be followed by `(` (skip whitespace) — i.e. a call.
        let mut tail = after;
        while tail < bytes.len() && (bytes[tail] == b' ' || bytes[tail] == b'\t') {
            tail += 1;
        }
        let next_ok = tail < bytes.len() && bytes[tail] == b'(';
        // Avoid matching the method's own declaration (e.g. `void method(...)`).
        // A declaration call site is preceded by a return type or modifier on
        // the same line; detect by walking back and checking whether the line
        // ends with `;` after the closing paren — skip when the line is a
        // declaration. A simple heuristic: if the call is preceded by a
        // return-type-shaped identifier on the same line, treat as decl. We
        // approximate by checking whether the same line contains `{` after
        // the call (decl bodies open with `{`) AND no `;` or `=`/`return`
        // before the call. Since target text contains the *bodies* of
        // extracted methods, declarations look like
        // `    void runStuff() {` whereas call sites look like
        // `        runStuff();` or `        x = runStuff();`.
        // Find line bounds.
        let line_start = target_text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = target_text[pos..]
            .find('\n')
            .map(|i| pos + i)
            .unwrap_or(bytes.len());
        let line = &target_text[line_start..line_end];
        // Heuristic: a method declaration line ends with `{` (after the
        // closing paren / throws clause). A call site ends with `;` or has
        // `;` somewhere after the `)`. Check the trimmed end.
        let trimmed_end = line.trim_end();
        let looks_like_decl = trimmed_end.ends_with('{');
        if !prev_ok || !next_ok || looks_like_decl {
            cursor = pos + needle.len();
            continue;
        }
        // Compute indentation of the call's line.
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        // Insert FIXME comment lines above this line.
        out.push_str(&target_text[copy_from..line_start]);
        for fixme in fixme_lines {
            out.push_str(&indent);
            out.push_str(fixme);
            out.push('\n');
        }
        copy_from = line_start;
        inserted += 1;
        // Advance cursor past this match so we don't double-process it.
        cursor = line_end;
    }
    out.push_str(&target_text[copy_from..]);
    (out, inserted)
}

/// Rewrite unqualified call sites of `method` in `text` to be qualified
/// by `class_name` (e.g. `myMethod(arg)` → `SourceClass.myMethod(arg)`).
///
/// G19-FU: AST-based rewrite via tree-sitter-java. Walks
/// `method_invocation` nodes with no `object` (receiver) field — those
/// are the unqualified calls. Strings, comments, and matching
/// occurrences inside larger identifiers are skipped by the parser
/// alone — they never produce a method_invocation node. Falls back to
/// the previous text-scan only when parsing fails (the target text is
/// always already parseable at this point, but the fallback keeps the
/// helper resilient to malformed inputs).
pub(crate) fn java_qualify_unqualified_calls(text: &str, method: &str, class_name: &str) -> String {
    if let Ok(tree) = parse_source("java", text) {
        let qualifier = format!("{class_name}.");
        let mut inserts: Vec<usize> = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let mut c = node.walk();
            for ch in node.named_children(&mut c) {
                stack.push(ch);
            }
            if node.kind() != "method_invocation" {
                continue;
            }
            // Unqualified call: no `object` field on the invocation.
            // (Receiver-bearing calls like `foo.method(...)` have one.)
            if node.child_by_field_name("object").is_some() {
                continue;
            }
            let Some(name_node) = node.child_by_field_name("name") else {
                continue;
            };
            let Ok(name_text) = name_node.utf8_text(text.as_bytes()) else {
                continue;
            };
            if name_text != method {
                continue;
            }
            inserts.push(name_node.start_byte());
        }
        // Apply inserts right-to-left so earlier offsets remain valid.
        inserts.sort_unstable();
        inserts.dedup();
        if inserts.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len() + inserts.len() * qualifier.len());
        let mut last = 0usize;
        for at in inserts {
            out.push_str(&text[last..at]);
            out.push_str(&qualifier);
            last = at;
        }
        out.push_str(&text[last..]);
        return out;
    }
    // Fallback: byte scan (pre-G19-FU behavior). Reached only on
    // unparseable input. The scan rejects matches preceded by an
    // identifier char / `.` / `:`, requires `(` after the name, and
    // skips method-declaration lines, but doesn't recognize string
    // literals or comments — so the AST path is strongly preferred.
    let bytes = text.as_bytes();
    let needle = method.as_bytes();
    let qualifier = format!("{class_name}.");
    let mut out = String::with_capacity(text.len() + 64);
    let mut cursor = 0usize;
    let mut copy_from = 0usize;
    while cursor + needle.len() <= bytes.len() {
        let Some(rel) = text[cursor..].find(method) else {
            break;
        };
        let pos = cursor + rel;
        let after = pos + needle.len();
        let prev_ok = if pos == 0 {
            true
        } else {
            let prev = bytes[pos - 1] as char;
            !(prev == '.'
                || prev == ':'
                || prev == '_'
                || prev == '$'
                || prev.is_ascii_alphanumeric())
        };
        let mut tail = after;
        while tail < bytes.len() && (bytes[tail] == b' ' || bytes[tail] == b'\t') {
            tail += 1;
        }
        let next_ok = tail < bytes.len() && bytes[tail] == b'(';
        // Skip the method's own declaration line — same heuristic as
        // java_insert_fixme_above_calls.
        let line_end = text[pos..]
            .find('\n')
            .map(|i| pos + i)
            .unwrap_or(bytes.len());
        let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = &text[line_start..line_end];
        let looks_like_decl = line.trim_end().ends_with('{');
        if !prev_ok || !next_ok || looks_like_decl {
            cursor = pos + needle.len();
            continue;
        }
        // Insert `<class_name>.` immediately before the method name.
        out.push_str(&text[copy_from..pos]);
        out.push_str(&qualifier);
        copy_from = pos;
        cursor = after;
    }
    out.push_str(&text[copy_from..]);
    out
}

/// Build the standard FIXME comment block for an unresolved external call.
fn fixme_external_call(method: &str) -> Vec<String> {
    vec![
        format!("// FIXME: external call `{method}` — unresolved on target. Source-class method."),
        "//   resolutions: add to extracted set, extract callback interface, inject source instance, or drop the call if it returns void and the source-side side effect does not apply to the target.".to_string(),
    ]
}

/// Build the standard FIXME comment block for an inherited dependency from a
/// superclass that the target does not extend.
fn fixme_inherited_class_call(method: &str, source: &str) -> Vec<String> {
    vec![
        format!("// FIXME: inherited call `{method}` — inherited from class {source} on the source. Extracted target does not extend {source}."),
        "//   resolutions: extend the same superclass, inject the dependency, or move the call back to the source.".to_string(),
    ]
}

/// Gap 29: insert a FIXME comment block above the generated
/// `private final <Type> <name>;` line in the target text, warning the
/// operator that the source field is non-final and the target only sees a
/// snapshot of the value taken at construction time. Idempotent: if the
/// FIXME marker already lives directly above the field declaration, the
/// input is returned unchanged. Greppable prefix: `// FIXME: mutable
/// capture `<name>``.
fn java_insert_fixme_above_mutable_capture(
    target_text: &str,
    capture: &CapturedVariable,
) -> String {
    // Locate the generated field line. The target body renders captures as
    // `    private final <Type> <Name>;` (see dependency_field_text). We
    // search for the substring ending in ` <name>;` that is preceded by
    // `final ` so we don't accidentally hit some other `final` site.
    let needle = format!("final {} {};", capture.source_type, capture.name);
    let Some(decl_at) = target_text.find(&needle) else {
        return target_text.to_string();
    };
    // Walk back to the line start to capture the indent.
    let line_start = target_text[..decl_at]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent: String = target_text[line_start..decl_at]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();

    // Idempotency: if the line directly above this declaration already
    // carries our FIXME marker for this capture, do nothing.
    let marker = format!("// FIXME: mutable capture `{}`", capture.name);
    if line_start > 0 {
        let preceding = &target_text[..line_start];
        if let Some(prev_line_start) = preceding[..preceding.len() - 1].rfind('\n') {
            if target_text[prev_line_start + 1..line_start].contains(&marker) {
                return target_text.to_string();
            }
        } else if preceding.contains(&marker) {
            return target_text.to_string();
        }
    }

    let comment = format!(
        "{indent}// FIXME: mutable capture `{name}` (source field is non-final). Promoted to `final` constructor param — value snapshotted at construction.\n\
         {indent}//   resolutions: use Supplier<{ty}>, shared holder, or keep on source and access via reference.\n",
        indent = indent,
        name = capture.name,
        ty = boxed_for_supplier(&capture.source_type),
    );
    let mut out = String::with_capacity(target_text.len() + comment.len());
    out.push_str(&target_text[..line_start]);
    out.push_str(&comment);
    out.push_str(&target_text[line_start..]);
    out
}

/// Render a Java type for use as the parameter of `Supplier<…>` — primitive
/// types must be boxed (Supplier doesn't accept primitives). Non-primitive
/// types pass through unchanged.
fn boxed_for_supplier(java_type: &str) -> String {
    match java_type.trim() {
        "boolean" => "Boolean".to_string(),
        "byte" => "Byte".to_string(),
        "short" => "Short".to_string(),
        "int" => "Integer".to_string(),
        "long" => "Long".to_string(),
        "float" => "Float".to_string(),
        "double" => "Double".to_string(),
        "char" => "Character".to_string(),
        other => other.to_string(),
    }
}

/// Collect only the ABSTRACT methods declared on an interface — methods
/// the implementer MUST provide. Excludes `default`, `static`, and
/// `private` methods (all of which have bodies on the interface itself
/// and are already satisfied). Used by the `implements`-completeness
/// check; without this filter, a target that "implements" an interface
/// whose only method is `default` incorrectly gets a
/// `// FIXME: target now implements ... but does not satisfy method(s)`
/// marker.
fn collect_interface_abstract_method_names(
    project_dir: &Path,
    interface_name: &str,
) -> Option<HashSet<String>> {
    let type_paths = project_java_type_paths(project_dir);
    let path = type_paths.get(interface_name)?.as_ref()?;
    let parsed = parse_source_file(path).ok()?;
    let type_node = find_java_type_declaration_by_name(&parsed, interface_name)?;
    let body = type_node.child_by_field_name("body").unwrap_or(type_node);
    let mut names = HashSet::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let mods = collect_java_modifiers(child);
        let has_concrete_modifier = mods
            .iter()
            .any(|(name, _, _)| matches!(name.as_str(), "default" | "static" | "private"));
        if has_concrete_modifier {
            continue;
        }
        // A method without a body is abstract on an interface; tree-sitter
        // exposes the body as a child_by_field_name("body") that's absent
        // for abstract methods. Belt-and-suspenders check.
        if child.child_by_field_name("body").is_some() {
            continue;
        }
        if let Some(name) = child.child_by_field_name("name") {
            if let Ok(text) = name.utf8_text(parsed.source.as_bytes()) {
                names.insert(text.to_string());
            }
        }
    }
    Some(names)
}

fn java_class_name(class_node: Node<'_>, source: &str) -> String {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or("(unnamed)")
        .to_string()
}

fn java_field_declaration_name(node: Node<'_>, source: &str) -> Option<String> {
    find_node(node, |n| {
        n.kind() == "variable_declarator" || n.kind() == "variable_declarator_id"
    })
    .and_then(|decl| {
        decl.child_by_field_name("name")
            .or_else(|| decl.child_by_field_name("declarator"))
            .or_else(|| {
                let mut cursor = decl.walk();
                let found = decl
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "identifier");
                found
            })
    })
    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
    .map(str::to_string)
}

fn java_field_type_text(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("type")
        .or_else(|| {
            let mut cursor = node.walk();
            let found = node.named_children(&mut cursor).find(|child| {
                !matches!(
                    child.kind(),
                    "modifiers" | "variable_declarator" | "variable_declarator_id"
                )
            });
            found
        })
        .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
        .map(|text| text.trim().to_string())
}

fn java_fields(parsed: &ParsedSource) -> Vec<JavaField> {
    let mut fields = Vec::new();
    let root = parsed.tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "field_declaration" {
            if let (Some(name), Some(type_name)) = (
                java_field_declaration_name(node, &parsed.source),
                java_field_type_text(node, &parsed.source),
            ) {
                let is_final = collect_java_modifiers(node)
                    .iter()
                    .any(|(name, _, _)| name == "final");
                fields.push(JavaField {
                    name,
                    type_name,
                    item: syntax_item_with_kind(parsed, node, "field_declaration"),
                    is_final,
                });
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    fields
}

fn java_class_body_insert_position(class_node: Node<'_>, source: &str) -> usize {
    find_open_brace_position(class_node, source) + 1
}

fn java_after_fields_insert_position(class_node: Node<'_>, source: &str) -> usize {
    let Some(body) = class_node.child_by_field_name("body") else {
        return java_class_body_insert_position(class_node, source);
    };
    let mut cursor = body.walk();
    let mut last_field_end = None;
    for child in body.named_children(&mut cursor) {
        if child.kind() == "field_declaration" {
            last_field_end = Some(child.end_byte());
        }
    }
    last_field_end.unwrap_or_else(|| java_class_body_insert_position(class_node, source))
}

fn validate_java_member_name(name: &str, field: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        bail!("{field} must be a valid Java identifier, got `{name}`");
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        bail!("{field} must be a valid Java identifier, got `{name}`");
    }
    Ok(())
}

fn validate_java_visibility(value: &str) -> Result<()> {
    if !matches!(value, "public" | "protected" | "private" | "package") {
        bail!("visibility must be one of: public, protected, private, package; got `{value}`");
    }
    Ok(())
}

fn java_field_decl(spec: &JavaFieldSpec) -> Result<String> {
    validate_java_member_name(&spec.name, "field name")?;
    if let Some(visibility) = spec.visibility.as_deref() {
        validate_java_visibility(visibility)?;
    }
    let mut parts = Vec::new();
    if let Some(visibility) = spec.visibility.as_deref().filter(|v| *v != "package") {
        parts.push(visibility.to_string());
    }
    if spec.final_field == Some(true) {
        parts.push("final".to_string());
    }
    parts.push(spec.type_name.trim().to_string());
    parts.push(spec.name.clone());
    Ok(format!("    {};\n", parts.join(" ")))
}

fn java_constructor_decl(
    class_name: &str,
    visibility: &str,
    params: &[JavaParameterSpec],
    assign_to_fields: bool,
    extra_statement: Option<&str>,
) -> Result<String> {
    validate_java_visibility(visibility)?;
    let params_text = params
        .iter()
        .map(|param| {
            validate_java_member_name(&param.name, "parameter name")?;
            Ok(format!("{} {}", param.type_name.trim(), param.name))
        })
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let prefix = if visibility == "package" {
        String::new()
    } else {
        format!("{visibility} ")
    };
    let mut out = format!("    {prefix}{class_name}({params_text}) {{\n");
    if assign_to_fields {
        for param in params {
            out.push_str(&format!("        this.{0} = {0};\n", param.name));
        }
    }
    if let Some(stmt) = extra_statement.filter(|stmt| !stmt.trim().is_empty()) {
        out.push_str("        ");
        out.push_str(stmt.trim().trim_end_matches(';'));
        out.push_str(";\n");
    }
    out.push_str("    }\n");
    Ok(out)
}

fn first_constructor_node<'a>(class_node: Node<'a>, source: &str) -> Option<Node<'a>> {
    let class_name = java_class_name(class_node, source);
    find_node(class_node, |node| {
        node.kind() == "constructor_declaration"
            && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .is_some_and(|name| name == class_name)
    })
}

fn constructor_body_insert_position(constructor: Node<'_>, source: &str) -> usize {
    constructor
        .child_by_field_name("body")
        .map(|body| body.start_byte() + 1)
        .unwrap_or_else(|| find_open_brace_position(constructor, source) + 1)
}

/// Gap 8: state carried between the wiring-insert decision and the
/// post-accessor-rewrite ordering-conflict diagnostic pass.
struct WiringInsertState {
    /// Index into `source_edits` of the wiring TextEdit.
    edit_idx: usize,
    /// Constructor body `[start, end)` byte range. None when the source
    /// constructor has no parsable body node.
    body_range: Option<(usize, usize)>,
}

/// Collect identifier names declared as parameters of the constructor.
fn constructor_parameter_names(constructor: Node<'_>, source: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(params) = constructor.child_by_field_name("parameters") else {
        return names;
    };
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        if child.kind() == "formal_parameter" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(text) = name_node.utf8_text(source.as_bytes()) {
                    names.insert(text.to_string());
                }
            }
        }
    }
    names
}

/// Gap 7: find the latest top-level statement in `constructor`'s body that
/// assigns to any field whose name appears in `field_names`. Returns the
/// byte position immediately after that statement (its terminating `;`),
/// suitable as a zero-width insertion point.
///
/// Returns `None` when no qualifying statement exists.
fn last_field_assign_end_in_constructor(
    constructor: Node<'_>,
    source: &str,
    field_names: &HashSet<&str>,
) -> Option<usize> {
    let body = constructor.child_by_field_name("body")?;
    let mut last_end: Option<usize> = None;
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        // Statements that wrap assignments. Java parses `field = expr;` as an
        // `expression_statement` containing an `assignment_expression`.
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let mut stmt_cursor = stmt.walk();
        let assign = stmt
            .named_children(&mut stmt_cursor)
            .find(|c| c.kind() == "assignment_expression");
        let Some(assign) = assign else { continue };
        let Some(left) = assign.child_by_field_name("left") else {
            continue;
        };
        let lhs_name = match left.kind() {
            "identifier" => left.utf8_text(source.as_bytes()).ok().map(str::to_string),
            "field_access" => {
                // `this.foo = ...` — verify receiver is `this`, take field name.
                let receiver_is_this = left
                    .child_by_field_name("object")
                    .map(|o| o.kind() == "this")
                    .unwrap_or(false);
                if !receiver_is_this {
                    None
                } else {
                    left.child_by_field_name("field")
                        .and_then(|f| f.utf8_text(source.as_bytes()).ok())
                        .map(str::to_string)
                }
            }
            _ => None,
        };
        let Some(lhs_name) = lhs_name else { continue };
        if !field_names.contains(lhs_name.as_str()) {
            continue;
        }
        // The expression_statement's end_byte includes the trailing `;`.
        let end = stmt.end_byte();
        last_end = Some(match last_end {
            Some(prev) if prev > end => prev,
            _ => end,
        });
    }
    last_end
}

fn java_modifier_text(node: Node<'_>, source: &str) -> String {
    let mods = collect_java_modifiers(node);
    if mods.is_empty() {
        "package".to_string()
    } else {
        mods.iter()
            .map(|(_, start, end)| source[*start..*end].trim().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Direct-child `field_declaration` nodes of the outermost class body. Inner
/// classes' field declarations are intentionally excluded — only the source
/// class's own fields can be captured by extracted methods.
fn outer_class_field_map(parsed: &ParsedSource) -> BTreeMap<String, JavaField> {
    let mut map = BTreeMap::new();
    let Some(class_node) = find_first_class_declaration(parsed.tree.root_node()) else {
        return map;
    };
    let Some(body) = class_node.child_by_field_name("body") else {
        return map;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        if let (Some(name), Some(type_name)) = (
            java_field_declaration_name(child, &parsed.source),
            java_field_type_text(child, &parsed.source),
        ) {
            let is_final = collect_java_modifiers(child)
                .iter()
                .any(|(name, _, _)| name == "final");
            map.insert(
                name.clone(),
                JavaField {
                    name,
                    type_name,
                    item: syntax_item_with_kind(parsed, child, "field_declaration"),
                    is_final,
                },
            );
        }
    }
    map
}

/// Walk every identifier inside `method_node` and return tuples of
/// (name, identifier_node, is_qualified_this_access). The third bool is true
/// when the identifier is the `field` part of a `this.<name>` field_access —
/// such accesses are never shadowable by enclosing locals/parameters.
fn collect_identifier_uses<'a>(
    node: Node<'a>,
    source: &str,
    out: &mut Vec<(String, Node<'a>, bool)>,
) {
    if node.kind() == "identifier" {
        if let Some(access_node) = resolve_field_access(node) {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                let qualified_this =
                    access_node.id() != node.id() && access_node.kind() == "field_access";
                out.push((text.to_string(), node, qualified_this));
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifier_uses(child, source, out);
    }
}

fn captured_fields_for_methods(
    parsed: &ParsedSource,
    selected: &[JavaMethod],
) -> Vec<CapturedVariable> {
    // Source-class fields only. Inner-class field declarations are not
    // captures of the source class — they are independent declarations.
    let fields = outer_class_field_map(parsed);
    if fields.is_empty() {
        return Vec::new();
    }

    // Resolve each identifier inside the selected methods against the
    // outer-class field map, applying the same shadowing rules used for
    // remaining-source-accessor analysis. An identifier counts as a capture
    // only when (a) its name maps to a source-class field AND (b) no
    // enclosing local/parameter/enhanced-for variable shadows it before the
    // method boundary.
    let mut captured_names: BTreeSet<String> = BTreeSet::new();
    for method in selected {
        let Some(method_node) = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        }) else {
            continue;
        };
        let mut uses = Vec::new();
        collect_identifier_uses(method_node, &parsed.source, &mut uses);
        for (name, ident_node, qualified_this) in uses {
            if !fields.contains_key(&name) {
                continue;
            }
            // `this.X` always resolves to `this`'s field regardless of
            // enclosing local/parameter shadows. Only bare-name reads can
            // be shadowed.
            if !qualified_this && is_shadowed(ident_node, &name, &parsed.source) {
                continue;
            }
            captured_names.insert(name);
        }
    }

    fields
        .into_iter()
        .filter(|(name, _)| captured_names.contains(name))
        .map(|(name, field)| {
            let field_node = find_node(parsed.tree.root_node(), |node| {
                node.kind() == "field_declaration"
                    && node.start_byte() == field.item.byte_start
                    && node.end_byte() == field.item.byte_end
            })
            .unwrap_or(parsed.tree.root_node());
            let mods = collect_java_modifiers(field_node);
            let has_final = mods.iter().any(|(name, _, _)| name == "final");
            let has_static = mods.iter().any(|(name, _, _)| name == "static");
            CapturedVariable {
                name,
                kind: "field".to_string(),
                source_type: field.type_name,
                source_visibility: java_modifier_text(field_node, &parsed.source),
                source_mutable: !has_final,
                source_static_final: has_static && has_final,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Extracted-dependency analysis (closes Java refactor gaps 12, 14, 15).
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct ExtractionDependencyReport {
    pub external_calls: Vec<ExternalCall>,
    pub inherited_dependencies: Vec<InheritedDependency>,
}

/// File-path index for the project's Java types. Distinct from
/// `project_java_type_index` (which yields fqcns) because we need the file
/// path to lazily reparse ancestor types when resolving inherited calls.
fn project_java_type_paths(project_dir: &Path) -> BTreeMap<String, Option<PathBuf>> {
    let mut index: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();
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
                Some("target" | "build" | ".gradle")
            )
        }) {
            continue;
        }
        let Some(simple) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match index.get_mut(simple) {
            Some(slot) => *slot = None,
            None => {
                index.insert(simple.to_string(), Some(path.to_path_buf()));
            }
        }
    }
    index
}

/// Collect the simple names of `extends` superclasses and `implements`
/// interfaces declared on a class node (one hop).
fn collect_java_super_type_names(class_node: Node<'_>, source: &str) -> Vec<String> {
    fn collect_type_identifiers(node: Node<'_>, source: &str, out: &mut Vec<String>) {
        if node.kind() == "type_identifier" {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                out.push(text.to_string());
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            collect_type_identifiers(child, source, out);
        }
    }
    let mut out = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "superclass" || kind == "interfaces" || kind == "super_interfaces" {
            collect_type_identifiers(child, source, &mut out);
        }
    }
    out
}

/// Collect public/protected/package method names declared directly on a
/// type (class, interface, abstract class) body. Includes default-impl
/// interface methods. Static methods are included — call resolution at the
/// extraction site can't tell static from instance without semantic data.
fn collect_java_type_method_names(parsed: &ParsedSource, type_node: Node<'_>) -> HashSet<String> {
    let mut names = HashSet::new();
    let body = type_node.child_by_field_name("body").unwrap_or(type_node);
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            if let Some(name) = child.child_by_field_name("name") {
                if let Ok(text) = name.utf8_text(parsed.source.as_bytes()) {
                    names.insert(text.to_string());
                }
            }
        }
    }
    names
}

/// Find any top-level type declaration with the given simple name in `parsed`.
fn find_java_type_declaration_by_name<'a>(
    parsed: &'a ParsedSource,
    type_name: &str,
) -> Option<Node<'a>> {
    let root = parsed.tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
        ) {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                    if name == type_name {
                        return Some(node);
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn java_type_kind_label(node: Node<'_>) -> &'static str {
    match node.kind() {
        "interface_declaration" => "interface",
        _ => "class",
    }
}

/// Method invocation discovered inside an extracted method body.
pub(crate) struct InvocationHit<'a> {
    name: String,
    /// True if the call has an explicit receiver (foo.bar(), this.bar(),
    /// SomeType.bar()). Only unqualified calls and `this.`-qualified calls
    /// can resolve to source-class methods, so we filter on this.
    has_explicit_other_receiver: bool,
    line: usize,
    column: usize,
    in_method: String,
    inside_lambda: bool,
    #[allow(dead_code)]
    node: Node<'a>,
}

/// Walk a method declaration's body and collect every method_invocation,
/// noting the enclosing method name and whether the call is inside a
/// lambda_expression ancestor before reaching the enclosing method.
pub(crate) fn collect_method_invocations<'a>(
    method_node: Node<'a>,
    enclosing_method_name: &str,
    parsed: &'a ParsedSource,
    out: &mut Vec<InvocationHit<'a>>,
) {
    fn walk<'a>(
        node: Node<'a>,
        enclosing_method_name: &str,
        method_start_byte: usize,
        parsed: &'a ParsedSource,
        in_lambda_depth: usize,
        out: &mut Vec<InvocationHit<'a>>,
    ) {
        let kind = node.kind();
        // Don't recurse into nested method declarations (anonymous inner
        // class methods); their bodies are their own enclosing method.
        if (kind == "method_declaration" || kind == "constructor_declaration")
            && node.start_byte() != method_start_byte
        {
            return;
        }
        let next_lambda_depth = if kind == "lambda_expression" {
            in_lambda_depth + 1
        } else {
            in_lambda_depth
        };
        if kind == "method_invocation" {
            // Detect explicit non-this receiver: object field is set and is
            // not `this`. Also cover identifier-only receivers.
            let mut has_explicit_other_receiver = false;
            if let Some(obj) = node.child_by_field_name("object") {
                let receiver_text = obj.utf8_text(parsed.source.as_bytes()).unwrap_or("").trim();
                if receiver_text != "this" {
                    has_explicit_other_receiver = true;
                }
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                    let (line, column) = line_col(&parsed.source, name_node.start_byte());
                    out.push(InvocationHit {
                        name: name.to_string(),
                        has_explicit_other_receiver,
                        line,
                        column,
                        in_method: enclosing_method_name.to_string(),
                        inside_lambda: in_lambda_depth > 0,
                        node,
                    });
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(
                child,
                enclosing_method_name,
                method_start_byte,
                parsed,
                next_lambda_depth,
                out,
            );
        }
    }
    walk(
        method_node,
        enclosing_method_name,
        method_node.start_byte(),
        parsed,
        0,
        out,
    );
}

/// Build a lightweight signature for a method declaration on the source
/// class. Returns (signature, partial). Partial is true when key fields
/// (return_type, parameters) couldn't be recovered cleanly.
fn java_method_signature_text(method_node: Node<'_>, source: &str) -> (String, bool) {
    let mut partial = false;
    let return_type = method_node
        .child_by_field_name("type")
        .or_else(|| method_node.child_by_field_name("return_type"))
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            partial = true;
            "?".to_string()
        });
    let name = method_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or_else(|| {
            partial = true;
            "?"
        });
    let params = method_node
        .child_by_field_name("parameters")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            partial = true;
            "(?)".to_string()
        });
    (format!("{return_type} {name}{params}"), partial)
}

/// For every method on the source class, return a map from method name to
/// (signature, partial). Constructors are intentionally excluded — they're
/// resolved via `new`, not method invocation.
/// G8: per-source-method metadata for ExternalCall classification.
#[derive(Debug, Clone)]
pub(crate) struct JavaSourceMethodInfo {
    pub signature: String,
    pub signature_partial: bool,
    pub visibility: String,
    pub is_static: bool,
}

pub(crate) fn java_source_class_method_signatures(
    parsed: &ParsedSource,
    class_node: Node<'_>,
) -> BTreeMap<String, JavaSourceMethodInfo> {
    let mut out: BTreeMap<String, JavaSourceMethodInfo> = BTreeMap::new();
    let body = class_node.child_by_field_name("body").unwrap_or(class_node);
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "method_declaration" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                    let (sig, partial) = java_method_signature_text(child, &parsed.source);
                    let visibility = java_method_visibility(child, &parsed.source);
                    let is_static = java_method_has_modifier(child, &parsed.source, "static");
                    // First definition wins; overloads collapse to one entry
                    // since we can't distinguish them at the call site without
                    // full type resolution.
                    out.entry(name.to_string()).or_insert(JavaSourceMethodInfo {
                        signature: sig,
                        signature_partial: partial,
                        visibility,
                        is_static,
                    });
                }
            }
        }
    }
    out
}

/// Return a method declaration's visibility — "public", "protected",
/// "private", or "package" for the default (no explicit modifier).
fn java_method_visibility(method_node: Node<'_>, source: &str) -> String {
    if java_method_has_modifier(method_node, source, "public") {
        "public".into()
    } else if java_method_has_modifier(method_node, source, "protected") {
        "protected".into()
    } else if java_method_has_modifier(method_node, source, "private") {
        "private".into()
    } else {
        "package".into()
    }
}

/// Whether a method declaration carries a specific modifier keyword.
fn java_method_has_modifier(method_node: Node<'_>, source: &str, keyword: &str) -> bool {
    let mut cursor = method_node.walk();
    for child in method_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut mc = child.walk();
        for modifier in child.children(&mut mc) {
            if let Ok(text) = modifier.utf8_text(source.as_bytes()) {
                if text == keyword {
                    return true;
                }
            }
        }
    }
    false
}

/// Walk `extends` / `implements` chains starting from a source class to
/// build a map of inherited method name -> (declaring type name, kind).
/// The first declaration found along BFS wins, mirroring Java's nearest-
/// ancestor resolution. Cycles are guarded via a visited set.
pub(crate) fn collect_inherited_method_declarations(
    project_dir: &Path,
    source_class_node: Node<'_>,
    parsed: &ParsedSource,
) -> BTreeMap<String, (String, String)> {
    let type_paths = project_java_type_paths(project_dir);
    let mut visited: HashSet<String> = HashSet::new();
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();

    // BFS: queue holds simple type names to expand.
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let class_name = java_class_name(source_class_node, &parsed.source);
    visited.insert(class_name);
    for super_name in collect_java_super_type_names(source_class_node, &parsed.source) {
        if visited.insert(super_name.clone()) {
            queue.push_back(super_name);
        }
    }

    while let Some(simple) = queue.pop_front() {
        let Some(Some(path)) = type_paths.get(&simple) else {
            continue; // Ambiguous (None) or absent — JDK / library / unknown.
        };
        let Ok(parsed_ancestor) = parse_source_file(path) else {
            continue;
        };
        let Some(type_node) = find_java_type_declaration_by_name(&parsed_ancestor, &simple) else {
            continue;
        };
        let kind_label = java_type_kind_label(type_node);
        for method in collect_java_type_method_names(&parsed_ancestor, type_node) {
            out.entry(method)
                .or_insert((simple.clone(), kind_label.to_string()));
        }
        for next in collect_java_super_type_names(type_node, &parsed_ancestor.source) {
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }

    out
}

/// Top-level analysis entry point. Walks the bodies of `selected` methods,
/// classifies every method invocation, and returns external + inherited
/// dependency reports. Internal calls (inside the extracted set) are
/// dropped. Library/JDK calls (unresolved against the project type index)
/// are also dropped.
pub(crate) fn analyze_extracted_dependencies(
    parsed: &ParsedSource,
    selected: &[JavaMethod],
    project_dir: Option<&Path>,
) -> ExtractionDependencyReport {
    let mut report = ExtractionDependencyReport::default();
    if selected.is_empty() {
        return report;
    }
    let Some(source_class_node) = find_first_class_declaration(parsed.tree.root_node()) else {
        return report;
    };

    let extracted_names: HashSet<String> = selected
        .iter()
        .filter_map(|m| m.item.name.clone())
        .collect();

    let source_methods = java_source_class_method_signatures(parsed, source_class_node);

    let inherited_methods: BTreeMap<String, (String, String)> = match project_dir {
        Some(dir) => collect_inherited_method_declarations(dir, source_class_node, parsed),
        None => BTreeMap::new(),
    };

    // Walk every selected method body once and collect invocations.
    let mut invocations: Vec<InvocationHit<'_>> = Vec::new();
    for method in selected {
        let Some(method_node) = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        }) else {
            continue;
        };
        let enclosing_name = method.item.name.clone().unwrap_or_default();
        collect_method_invocations(method_node, &enclosing_name, parsed, &mut invocations);
    }

    let mut external: BTreeMap<String, ExternalCall> = BTreeMap::new();
    let mut inherited: BTreeMap<String, InheritedDependency> = BTreeMap::new();

    for hit in invocations {
        // Calls with explicit non-this receivers cannot bind to the source
        // class's own methods or to the source class's inherited methods; skip.
        if hit.has_explicit_other_receiver {
            continue;
        }
        // Internal: in extracted set. Drop.
        if extracted_names.contains(&hit.name) {
            continue;
        }
        let context = if hit.inside_lambda {
            "lambda"
        } else {
            "direct"
        };
        let site = ExtractedCallSite {
            line: hit.line,
            column: hit.column,
            in_method: hit.in_method.clone(),
            context: context.to_string(),
        };
        if let Some(info) = source_methods.get(&hit.name) {
            // External: declared on source class body.
            // G8: surface source method visibility + is_static, plus a
            // recommended_resolution hint. Public-static externals get
            // `cross_class_static_call` (auto-resolved at apply by G19).
            // Private externals can only realistically be moved with
            // the cluster, so recommend `add_to_item_names`.
            let recommended = if info.is_static && info.visibility == "public" {
                Some("cross_class_static_call".to_string())
            } else if info.visibility == "private" {
                Some("add_to_item_names".to_string())
            } else {
                None
            };
            let entry = external
                .entry(hit.name.clone())
                .or_insert_with(|| ExternalCall {
                    method: hit.name.clone(),
                    signature: info.signature.clone(),
                    signature_partial: info.signature_partial,
                    source_visibility: Some(info.visibility.clone()),
                    source_is_static: info.is_static,
                    recommended_resolution: recommended,
                    call_sites: Vec::new(),
                });
            entry.call_sites.push(site);
        } else if let Some((source, kind)) = inherited_methods.get(&hit.name) {
            let entry = inherited
                .entry(hit.name.clone())
                .or_insert_with(|| InheritedDependency {
                    method: hit.name.clone(),
                    source: source.clone(),
                    source_kind: kind.clone(),
                    call_sites: Vec::new(),
                });
            entry.call_sites.push(site);
        }
        // Else: unresolved → JDK / library / unknown. Drop.
    }

    report.external_calls = external.into_values().collect();
    report.inherited_dependencies = inherited.into_values().collect();
    report
}

fn select_java_methods_by_name(parsed: &ParsedSource, names: &[String]) -> Result<Vec<JavaMethod>> {
    if names.is_empty() {
        bail!("item_names (method names) must be provided");
    }
    let candidates = java_methods(parsed);
    if candidates.is_empty() {
        bail!("no Java methods found");
    }
    let mut selected = Vec::new();
    for expected in names {
        // Parse a signature suffix for overload disambiguation:
        // `methodName(Type1,Type2)` — match by (name, parameter-type list).
        // Bare `methodName` still works when the name is unique.
        let (target_name, sig_params): (&str, Option<Vec<String>>) = match expected.find('(') {
            Some(open) if expected.ends_with(')') => {
                let name = expected[..open].trim();
                let inside = &expected[open + 1..expected.len() - 1];
                let params: Vec<String> = if inside.trim().is_empty() {
                    Vec::new()
                } else {
                    inside
                        .split(',')
                        .map(normalize_param_type_text)
                        .collect()
                };
                (name, Some(params))
            }
            _ => (expected.as_str(), None),
        };
        let name_matches: Vec<&JavaMethod> = candidates
            .iter()
            .filter(|m| m.item.name.as_deref() == Some(target_name))
            .collect();
        match name_matches.as_slice() {
            [] => {
                let nested_match = java_nested_classes(parsed)
                    .into_iter()
                    .any(|c| c.item.name.as_deref() == Some(target_name));
                if nested_match {
                    bail!(
                        "error.bad_input(code=nested_class_in_item_names): \
                         `{expected}` is a nested class, not a method. \
                         `extract_java_class` accepts only method names in `item_names`; \
                         inner-class extraction is not currently supported by this plan kind. \
                         Extract the inner class to a top-level file manually, then re-run \
                         extract_java_class for the outer methods."
                    );
                }
                bail!("requested method `{expected}` was not found");
            }
            [method] => selected.push((**method).clone()),
            multi => {
                // Overloaded — require a signature suffix.
                let Some(wanted) = sig_params.as_ref() else {
                    let overloads: Vec<String> = multi
                        .iter()
                        .map(|m| {
                            let params = java_method_param_types(parsed, m)
                                .unwrap_or_else(|| "?".to_string());
                            format!("{target_name}({params})")
                        })
                        .collect();
                    bail!(
                        "error.bad_input(code=method_overload_ambiguous): \
                         `{expected}` matched {n} overloads. Disambiguate by passing the \
                         signature suffix in `item_names`, e.g. {choices:?}.",
                        n = multi.len(),
                        choices = overloads
                    );
                };
                let chosen: Vec<&JavaMethod> = multi
                    .iter()
                    .copied()
                    .filter(|m| java_method_matches_param_types(parsed, m, wanted))
                    .collect();
                match chosen.as_slice() {
                    [m] => selected.push((**m).clone()),
                    [] => bail!(
                        "error.bad_input(code=method_overload_no_match): \
                         no overload of `{target_name}` matches param types {wanted:?}. \
                         Available overloads: {overloads:?}",
                        overloads = multi
                            .iter()
                            .map(|m| {
                                let params = java_method_param_types(parsed, m)
                                    .unwrap_or_else(|| "?".to_string());
                                format!("{target_name}({params})")
                            })
                            .collect::<Vec<_>>()
                    ),
                    _ => bail!(
                        "error.bad_input(code=method_overload_signature_collision): \
                         multiple overloads of `{target_name}` match {wanted:?} (likely the \
                         same param-type spelling appears twice)"
                    ),
                }
            }
        }
    }
    Ok(selected)
}

/// Return a comma-separated string of a method's parameter type texts —
/// e.g. `String, int` for `void foo(String a, int b)`. Used to render
/// the operator-facing overload list when item_names is ambiguous.
fn java_method_param_types(parsed: &ParsedSource, method: &JavaMethod) -> Option<String> {
    let method_node = find_node(parsed.tree.root_node(), |node| {
        matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) && node.start_byte() == method.item.byte_start
            && node.end_byte() == method.item.byte_end
    })?;
    let params = method_node.child_by_field_name("parameters")?;
    let mut cursor = params.walk();
    let parts: Vec<String> = params
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "formal_parameter")
        .filter_map(|p| {
            p.child_by_field_name("type")
                .and_then(|t| t.utf8_text(parsed.source.as_bytes()).ok())
                .map(|s| s.trim().to_string())
        })
        .collect();
    Some(parts.join(", "))
}

/// True when `method`'s parameter list matches `wanted_types` (one entry per
/// parameter, in order, comparing normalized type text). Generic parameter
/// types and `final` modifiers are normalized away before comparison.
fn java_method_matches_param_types(
    parsed: &ParsedSource,
    method: &JavaMethod,
    wanted_types: &[String],
) -> bool {
    let Some(method_node) = find_node(parsed.tree.root_node(), |node| {
        matches!(
            node.kind(),
            "method_declaration" | "constructor_declaration"
        ) && node.start_byte() == method.item.byte_start
            && node.end_byte() == method.item.byte_end
    }) else {
        return false;
    };
    let Some(params) = method_node.child_by_field_name("parameters") else {
        return false;
    };
    let mut cursor = params.walk();
    let actual: Vec<String> = params
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "formal_parameter")
        .filter_map(|p| {
            p.child_by_field_name("type")
                .and_then(|t| t.utf8_text(parsed.source.as_bytes()).ok())
                .map(normalize_param_type_text)
        })
        .collect();
    if actual.len() != wanted_types.len() {
        return false;
    }
    actual.iter().zip(wanted_types.iter()).all(|(a, w)| a == w)
}

/// Normalize a Java type string for overload matching: strip `final`,
/// collapse all whitespace, drop common annotation prefixes. The result
/// is whatever the operator can reasonably type without copying tabs and
/// `@Nullable` markers off the declaration.
fn normalize_param_type_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    let out = out.trim().to_string();
    // Strip a leading `final ` modifier — common in method-param decls but
    // not meaningful for overload resolution.
    let stripped = out
        .strip_prefix("final ")
        .map(|s| s.to_string())
        .unwrap_or(out);
    // Strip any leading `@…` annotation tokens.
    let mut rest = stripped.as_str();
    while let Some(stripped) = rest.strip_prefix('@') {
        // Skip until next whitespace.
        if let Some(idx) = stripped.find(char::is_whitespace) {
            rest = stripped[idx..].trim_start();
        } else {
            rest = "";
            break;
        }
    }
    rest.to_string()
}

fn select_java_fields_by_name(parsed: &ParsedSource, names: &[String]) -> Result<Vec<JavaField>> {
    let fields = java_fields(parsed);
    let mut selected = Vec::new();
    for expected in names {
        let matches = fields
            .iter()
            .filter(|field| field.name == *expected)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested field `{expected}` was not found"),
            [field] => selected.push((**field).clone()),
            _ => bail!("requested field `{expected}` matched multiple fields"),
        }
    }
    Ok(selected)
}

fn method_is_public(method_node: Node<'_>) -> bool {
    has_java_modifier(method_node, "public")
}

fn method_is_static(method_node: Node<'_>) -> bool {
    has_java_modifier(method_node, "static")
}

fn has_java_modifier(node: Node<'_>, modifier: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == modifier {
            return true;
        }
        if child.kind() == "modifiers" {
            let mut mc = child.walk();
            for mod_child in child.children(&mut mc) {
                if mod_child.kind() == modifier {
                    return true;
                }
            }
        }
    }
    false
}

fn collect_java_modifiers(node: Node<'_>) -> Vec<(String, usize, usize)> {
    let mut mods = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "modifiers" {
            let mut mc = child.walk();
            for mod_child in child.children(&mut mc) {
                let mk = mod_child.kind();
                if matches!(
                    mk,
                    "public"
                        | "protected"
                        | "private"
                        | "static"
                        | "final"
                        | "abstract"
                        | "synchronized"
                        | "native"
                        | "strictfp"
                        | "default"
                        | "transient"
                        | "volatile"
                ) {
                    mods.push((mk.to_string(), mod_child.start_byte(), mod_child.end_byte()));
                }
            }
            break;
        }
        if matches!(
            k,
            "public"
                | "protected"
                | "private"
                | "static"
                | "final"
                | "abstract"
                | "synchronized"
                | "native"
                | "strictfp"
                | "default"
                | "transient"
                | "volatile"
        ) {
            mods.push((k.to_string(), child.start_byte(), child.end_byte()));
        }
        if !matches!(k, "marker_annotation" | "annotation" | "modifiers") {
            break;
        }
    }
    mods
}

fn method_annotations_before(method_node: Node<'_>, source: &str) -> String {
    let start = method_node.start_byte();
    let mut cursor = method_node.walk();
    let first_non_annotation = method_node.children(&mut cursor).find(|child| {
        let k = child.kind();
        k != "marker_annotation" && k != "annotation"
    });
    let effective_start = first_non_annotation
        .map(|c| c.start_byte())
        .unwrap_or(start);
    let prefix = &source[..effective_start];
    let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let annotation_block = &source[line_start..effective_start];
    let annotations: Vec<&str> = annotation_block
        .lines()
        .filter(|l| {
            let t = l.trim();
            t.starts_with('@')
        })
        .collect();
    annotations.join("\n")
}

fn method_throws_text(method_node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = method_node.walk();
    for child in method_node.children(&mut cursor) {
        if child.kind() == "throws" {
            return child.utf8_text(source.as_bytes()).ok().map(str::to_string);
        }
    }
    None
}

fn method_type_parameters_text(method_node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = method_node.walk();
    for child in method_node.children(&mut cursor) {
        if child.kind() == "type_parameters" {
            return child.utf8_text(source.as_bytes()).ok().map(str::to_string);
        }
    }
    None
}

fn extract_interface_method_signature(method_node: Node<'_>, source: &str) -> String {
    let mut parts = Vec::new();
    let annotations = method_annotations_before(method_node, source);
    if !annotations.is_empty() {
        parts.push(annotations);
    }
    if let Some(tp) = method_type_parameters_text(method_node, source) {
        parts.push(tp);
    }
    let mut sig_parts = Vec::new();
    let mut cursor = method_node.walk();
    for child in method_node.children(&mut cursor) {
        let k = child.kind();
        if k == "modifiers" {
            let mut mc = child.walk();
            for mod_child in child.children(&mut mc) {
                let mk = mod_child.kind();
                if mk == "public" || mk == "protected" || mk == "private" {
                    continue;
                }
                if mk == "static" || mk == "native" || mk == "strictfp" {
                    continue;
                }
                if let Ok(text) = mod_child.utf8_text(source.as_bytes()) {
                    sig_parts.push(text.to_string());
                }
            }
            continue;
        }
        if k == "public" || k == "protected" || k == "private" {
            continue;
        }
        if k == "marker_annotation" || k == "annotation" {
            continue;
        }
        if k == "type_parameters" {
            continue;
        }
        if k == "body" || k == "block" {
            break;
        }
        if let Ok(text) = child.utf8_text(source.as_bytes()) {
            sig_parts.push(text.to_string());
        }
    }
    let mut sig = sig_parts.join(" ");
    if let Some(throws) = method_throws_text(method_node, source) {
        let throws_pos = sig.find("throws").unwrap_or(sig.len());
        if throws_pos >= sig.len() {
            let brace_pos = sig.find('{').unwrap_or(sig.len());
            if brace_pos < sig.len() {
                sig.insert_str(brace_pos, &format!(" {} ", throws));
            } else {
                sig = format!("{} {}", sig.trim_end_matches('{').trim(), throws);
            }
        }
    }
    let has_semi = sig.contains(';');
    let has_brace = sig.contains('{');
    if has_brace && !has_semi {
        if let Some(pos) = sig.find('{') {
            sig = format!("{};", sig[..pos].trim());
        }
    } else if !has_semi {
        sig = format!("{};", sig.trim());
    }
    parts.push(sig);
    parts.join("\n")
}

fn class_type_parameters_text(class_node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() == "type_parameters" {
            return child.utf8_text(source.as_bytes()).ok().map(str::to_string);
        }
    }
    None
}

fn find_implements_position(class_node: Node<'_>) -> Option<usize> {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() == "interfaces" || child.kind() == "superclass" {
            return Some(child.end_byte());
        }
    }
    None
}

fn find_open_brace_position(class_node: Node<'_>, source: &str) -> usize {
    let body = class_node.child_by_field_name("body");
    if let Some(body_node) = body {
        let pos = body_node.start_byte();
        if pos > 0 && source.as_bytes()[pos - 1] == b'{' {
            return pos - 1;
        }
        pos
    } else {
        class_node.start_byte()
    }
}

fn collect_type_params_in_signature(method_node: Node<'_>, source: &str) -> HashSet<String> {
    let mut tps = HashSet::new();
    fn walk_for_type_identifiers(node: Node<'_>, source: &str, tps: &mut HashSet<String>) {
        if node.kind() == "type_identifier" {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                let is_keyword = matches!(
                    text,
                    "void"
                        | "int"
                        | "long"
                        | "short"
                        | "byte"
                        | "float"
                        | "double"
                        | "boolean"
                        | "char"
                        | "String"
                        | "var"
                        | "List"
                        | "Map"
                        | "Set"
                        | "Collection"
                        | "Optional"
                        | "Stream"
                        | "Iterator"
                        | "Iterable"
                        | "Comparable"
                        | "Comparator"
                        | "Class"
                        | "Object"
                );
                if !is_keyword && text.chars().next().is_some_and(|c| c.is_uppercase()) {
                    tps.insert(text.to_string());
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_for_type_identifiers(child, source, tps);
        }
    }
    if let Some(ret_type) = method_node.child_by_field_name("return_type") {
        walk_for_type_identifiers(ret_type, source, &mut tps);
    }
    if let Some(params) = method_node.child_by_field_name("parameters") {
        walk_for_type_identifiers(params, source, &mut tps);
    }
    if let Some(throws) = method_node
        .children(&mut method_node.walk())
        .find(|c| c.kind() == "throws")
    {
        walk_for_type_identifiers(throws, source, &mut tps);
    }
    tps
}

fn is_type_use_context(node: Node<'_>, source: &str, class_name: &str) -> bool {
    let kind = node.kind();
    if kind != "type_identifier" {
        return false;
    }
    if let Ok(text) = node.utf8_text(source.as_bytes()) {
        if text != class_name {
            return false;
        }
    } else {
        return false;
    }
    let parent = match node.parent() {
        Some(p) => p,
        None => return false,
    };
    let parent_kind = parent.kind();
    if matches!(
        parent_kind,
        "object_creation_expression"
            | "class_literal"
            | "instanceof_expression"
            | "cast_expression"
            | "type_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "extends"
            | "implements"
    ) {
        return false;
    }
    if parent_kind == "method_declaration" {
        if let Some(name_node) = parent.child_by_field_name("name") {
            let name_pos = name_node.start_byte();
            if node.start_byte() > name_pos {
                return false;
            }
        }
    }
    if parent_kind == "method_invocation" {
        return false;
    }
    if parent_kind == "field_access" {
        return false;
    }
    true
}

fn find_type_use_positions_in_file(
    source: &str,
    tree: &Tree,
    class_name: &str,
) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    fn walk_node(
        node: Node<'_>,
        source: &str,
        class_name: &str,
        positions: &mut Vec<(usize, usize)>,
    ) {
        if is_type_use_context(node, source, class_name) {
            positions.push((node.start_byte(), node.end_byte()));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_node(child, source, class_name, positions);
        }
    }
    walk_node(tree.root_node(), source, class_name, &mut positions);
    positions.sort_by_key(|(start, _)| *start);
    positions
}

#[derive(Debug, Clone)]
struct JavaStaticFinalField {
    field: JavaField,
    type_text: String,
    declarator_text: String,
    visibility: String,
}

fn select_java_static_final_fields_by_name(
    parsed: &ParsedSource,
    names: &[String],
) -> Result<Vec<JavaStaticFinalField>> {
    let fields = java_fields(parsed);
    let mut selected = Vec::new();
    for expected in names {
        let matches: Vec<&JavaField> = fields
            .iter()
            .filter(|field| field.name == *expected)
            .collect();
        let field = match matches.as_slice() {
            [] => bail!("requested constant `{expected}` was not found"),
            [field] => (*field).clone(),
            _ => bail!("requested constant `{expected}` matched multiple field declarations"),
        };
        let node = find_node(parsed.tree.root_node(), |n| {
            n.kind() == "field_declaration"
                && n.start_byte() == field.item.byte_start
                && n.end_byte() == field.item.byte_end
        })
        .ok_or_else(|| anyhow!("could not locate AST node for constant `{expected}`"))?;
        let mods = collect_java_modifiers(node);
        let has_static = mods.iter().any(|(name, _, _)| name == "static");
        let has_final = mods.iter().any(|(name, _, _)| name == "final");
        if !(has_static && has_final) {
            bail!(
                "constant `{expected}` is not declared as `static final`; use move_java_field for instance fields"
            );
        }
        let type_node = node
            .child_by_field_name("type")
            .or_else(|| {
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
                children.into_iter().find(|child| {
                    !matches!(
                        child.kind(),
                        "modifiers" | "variable_declarator" | "variable_declarator_id"
                    )
                })
            })
            .ok_or_else(|| anyhow!("could not locate type node for constant `{expected}`"))?;
        let type_text = type_node
            .utf8_text(parsed.source.as_bytes())
            .map(|t| t.trim().to_string())
            .map_err(|e| anyhow!("invalid utf8 in type for `{expected}`: {e}"))?;
        let declarator_node = {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            children.into_iter().find(|child| {
                child.kind() == "variable_declarator" || child.kind() == "variable_declarator_id"
            })
        }
        .ok_or_else(|| anyhow!("could not locate declarator for constant `{expected}`"))?;
        let declarator_text = declarator_node
            .utf8_text(parsed.source.as_bytes())
            .map(|t| t.trim().to_string())
            .map_err(|e| anyhow!("invalid utf8 in declarator for `{expected}`: {e}"))?;
        selected.push(JavaStaticFinalField {
            field,
            type_text,
            declarator_text,
            visibility: java_visibility_from_mods(&mods).to_string(),
        });
    }
    Ok(selected)
}

fn render_java_static_final_with_visibility(
    info: &JavaStaticFinalField,
    visibility: &str,
) -> String {
    let mut parts = Vec::new();
    if visibility != "package" {
        parts.push(visibility.to_string());
    }
    parts.push("static".to_string());
    parts.push("final".to_string());
    parts.push(info.type_text.clone());
    parts.push(format!("{};", info.declarator_text));
    parts.join(" ")
}

/// Return the byte offset of the position right after the first `\n` at or
/// after `start` (or `source.len()` if no newline is found). This stops at
/// the line boundary instead of greedily consuming the next line's
/// indentation, which keeps consecutive removals from overlapping.
fn end_of_line_after(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = start;
    while idx < bytes.len() {
        let b = bytes[idx];
        idx += 1;
        if b == b'\n' {
            return idx;
        }
    }
    bytes.len()
}

/// Visibility ranking — higher means more visible to outside callers.
fn java_visibility_rank(v: &str) -> u8 {
    match v {
        "private" => 0,
        "package" => 1,
        "protected" => 2,
        "public" => 3,
        _ => 0,
    }
}

/// When keep_copy=true and the source-side declaration is tighter than
/// `package`, rewrite its visibility to `package` so siblings in the source
/// class continue to compile.
fn widen_static_final_visibility_edit(
    info: &JavaStaticFinalField,
    source: &str,
) -> Option<TextEdit> {
    if java_visibility_rank(&info.visibility) >= java_visibility_rank("package") {
        return None;
    }
    // Find the field_declaration node so we can collect modifiers.
    // We re-derive modifiers from the source slice rather than re-parsing —
    // collect_java_modifiers only needs the node, but here we already know
    // the byte range and visibility. Build a TextEdit that strips the
    // explicit `private` / `protected` token and any trailing space.
    let item_start = info.field.item.byte_start;
    let item_end = info.field.item.byte_end;
    let bytes = source.as_bytes();
    let slice = &source[item_start..item_end];
    let needle = info.visibility.as_str(); // "private" or "protected"
    let local_pos = slice.find(needle)?;
    let abs_start = item_start + local_pos;
    let mut abs_end = abs_start + needle.len();
    if bytes
        .get(abs_end)
        .copied()
        .map(|b| b == b' ' || b == b'\t')
        .unwrap_or(false)
    {
        abs_end += 1;
    }
    Some(TextEdit {
        byte_start: abs_start,
        byte_end: abs_end,
        replacement: String::new(),
    })
}

/// Walk the source AST after the moved field declarations are known, and
/// collect every remaining identifier read/write of those fields that stays
/// in the source class. Skips occurrences inside the moved declarations
/// themselves and identifiers shadowed by an enclosing local variable or
/// formal parameter of the same name.
/// Find every source-side reference to a moved static-final constant that
/// survives outside the extracted-method / moved-declaration ranges. Returns
/// one entry per constant. Each entry's `accesses` list is the call sites
/// that would fail to compile against a bare `CONST` reference after the
/// declaration moves to the target — these are the references the planner
/// rewrites to `<TargetClass>.<CONST>`.
///
/// `skip_ranges` should include both the extracted method bodies AND the
/// moved-constant declaration ranges (those are about to be deleted).
fn compute_remaining_source_constant_refs(
    parsed: &ParsedSource,
    constant_names: &[String],
    skip_ranges: &[(usize, usize)],
) -> Vec<RemainingFieldAccessor> {
    let name_set: HashSet<&str> = constant_names.iter().map(String::as_str).collect();
    let mut by_const: BTreeMap<String, Vec<FieldAccessSite>> = BTreeMap::new();
    for name in constant_names {
        by_const.insert(name.clone(), Vec::new());
    }
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" {
            continue;
        }
        let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
            continue;
        };
        if !name_set.contains(text) {
            continue;
        }
        if skip_ranges
            .iter()
            .any(|(s, e)| node.start_byte() >= *s && node.end_byte() <= *e)
        {
            continue;
        }
        // Exclude qualified accesses on something other than `this` — e.g.
        // `Other.CONST` already resolves elsewhere. Constants are accessed
        // bare or as `this.CONST`; both are handled by `resolve_field_access`.
        if resolve_field_access(node).is_none() {
            continue;
        }
        if is_shadowed(node, text, &parsed.source) {
            continue;
        }
        let (line, column) = line_col(&parsed.source, node.start_byte());
        let context = surrounding_context(&parsed.source, node.start_byte());
        if let Some(list) = by_const.get_mut(text) {
            list.push(FieldAccessSite {
                line,
                column,
                kind: "read".to_string(),
                context,
            });
        }
    }
    let mut report: Vec<RemainingFieldAccessor> = constant_names
        .iter()
        .map(|name| {
            let mut accesses = by_const.remove(name).unwrap_or_default();
            accesses.sort_by(|a, b| a.line.cmp(&b.line).then(a.column.cmp(&b.column)));
            RemainingFieldAccessor {
                field: name.clone(),
                accesses,
            }
        })
        .collect();
    report.sort_by_key(|r| {
        constant_names
            .iter()
            .position(|name| name == &r.field)
            .unwrap_or(usize::MAX)
    });
    report
}

/// Emit `CONST` → `<TargetClass>.<CONST>` rewrite edits for every surviving
/// source-side reference to a moved static-final constant. Skips refs inside
/// extracted methods and inside the moved declarations themselves (those are
/// about to be deleted) and shadowed refs.
fn compute_remaining_constant_qualify_edits(
    parsed: &ParsedSource,
    constant_names: &[String],
    skip_ranges: &[(usize, usize)],
    target_class_name: &str,
) -> Vec<TextEdit> {
    let name_set: HashSet<&str> = constant_names.iter().map(String::as_str).collect();
    let mut edits = Vec::new();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" {
            continue;
        }
        let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
            continue;
        };
        if !name_set.contains(text) {
            continue;
        }
        if skip_ranges
            .iter()
            .any(|(s, e)| node.start_byte() >= *s && node.end_byte() <= *e)
        {
            continue;
        }
        let Some(access_node) = resolve_field_access(node) else {
            continue;
        };
        if is_shadowed(node, text, &parsed.source) {
            continue;
        }
        // For bare reads: replace the identifier itself.
        // For `this.CONST`: replace the entire field_access expression
        // (drops the `this.` prefix and inserts the class qualifier).
        edits.push(TextEdit {
            byte_start: access_node.start_byte(),
            byte_end: access_node.end_byte(),
            replacement: format!("{target_class_name}.{text}"),
        });
    }
    edits
}

fn compute_remaining_source_accessors(
    parsed: &ParsedSource,
    field_names: &[String],
    moved_decl_ranges: &[(usize, usize)],
) -> Vec<RemainingFieldAccessor> {
    let name_set: HashSet<&str> = field_names.iter().map(String::as_str).collect();
    let mut by_field: BTreeMap<String, Vec<FieldAccessSite>> = BTreeMap::new();
    for name in field_names {
        by_field.insert(name.clone(), Vec::new());
    }
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" {
            continue;
        }
        let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
            continue;
        };
        if !name_set.contains(text) {
            continue;
        }
        // Filter occurrences inside the moved declarations.
        if moved_decl_ranges
            .iter()
            .any(|(s, e)| node.start_byte() >= *s && node.end_byte() <= *e)
        {
            continue;
        }
        // Resolve to a field-access expression: either the bare identifier or
        // an enclosing `this.<name>` field_access. Reject all other contexts
        // (method names, type names, declarators, qualified accesses on other
        // objects, etc.).
        let access_node = match resolve_field_access(node) {
            Some(n) => n,
            None => continue,
        };
        // Shadowing check: walk up looking for a local variable or formal
        // parameter with the same name in scope, stopping at class_body.
        if is_shadowed(node, text, &parsed.source) {
            continue;
        }
        let kind = classify_access_kind(access_node);
        let (line, column) = line_col(&parsed.source, access_node.start_byte());
        let context = surrounding_context(&parsed.source, access_node.start_byte());
        if let Some(list) = by_field.get_mut(text) {
            list.push(FieldAccessSite {
                line,
                column,
                kind: kind.to_string(),
                context,
            });
        }
    }
    let mut report: Vec<RemainingFieldAccessor> = field_names
        .iter()
        .map(|name| {
            let mut accesses = by_field.remove(name).unwrap_or_default();
            accesses.sort_by(|a, b| a.line.cmp(&b.line).then(a.column.cmp(&b.column)));
            RemainingFieldAccessor {
                field: name.clone(),
                accesses,
            }
        })
        .collect();
    // Stable: order matches input order.
    report.sort_by_key(|r| {
        field_names
            .iter()
            .position(|name| name == &r.field)
            .unwrap_or(usize::MAX)
    });
    report
}

// ---------------------------------------------------------------------------
// Gap 18: rewrite remaining source-side accesses through the delegate.
// ---------------------------------------------------------------------------

/// Per-field metadata used to drive accessor generation on the target and
/// rewrite the source-side accesses through the delegate.
#[derive(Debug, Clone)]
struct DelegateAccessorSpec {
    field_name: String,
    type_name: String,
    is_final: bool,
}

impl DelegateAccessorSpec {
    fn from_field(field: &JavaField) -> Self {
        Self {
            field_name: field.name.clone(),
            type_name: field.type_name.clone(),
            is_final: field.is_final,
        }
    }

    /// Method-name fragment used after `get`/`set`. PascalCases the field
    /// name unless the field already starts with an accessor-style prefix
    /// (`is*` / `has*`), in which case the bare field name is the getter.
    fn boolean_accessor(&self) -> Option<&'static str> {
        let name = self.field_name.as_str();
        let is_boolean = matches!(self.type_name.as_str(), "boolean" | "Boolean");
        if !is_boolean {
            return None;
        }
        if let Some(rest) = name.strip_prefix("is") {
            if rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            {
                return Some("is");
            }
        }
        if let Some(rest) = name.strip_prefix("has") {
            if rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
            {
                return Some("has");
            }
        }
        None
    }

    /// Getter method name, no parens.
    fn getter_name(&self) -> String {
        if self.boolean_accessor().is_some() {
            return self.field_name.clone();
        }
        format!("get{}", capitalize_first(&self.field_name))
    }

    /// Setter method name, no parens. Returns None when the field is
    /// `final` (no setter is generated and writes can't be rewritten).
    fn setter_name(&self) -> Option<String> {
        if self.is_final {
            return None;
        }
        // Boolean is/has prefix: setter name uses the simple-name form
        // (set + capitalised full name) per Java bean conventions.
        Some(format!("set{}", capitalize_first(&self.field_name)))
    }
}

fn capitalize_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Render the getter (and optionally setter) method declarations for one
/// moved field, using the visibility floor decided by the caller.
fn render_delegate_accessors(spec: &DelegateAccessorSpec, visibility: &str) -> String {
    let prefix = if visibility == "package" {
        String::new()
    } else {
        format!("{visibility} ")
    };
    let mut out = String::new();
    let getter = spec.getter_name();
    out.push_str(&format!(
        "    {prefix}{ty} {getter}() {{\n        return this.{field};\n    }}\n",
        ty = spec.type_name,
        field = spec.field_name,
    ));
    if let Some(setter) = spec.setter_name() {
        out.push('\n');
        out.push_str(&format!(
            "    {prefix}void {setter}({ty} {field}) {{\n        this.{field} = {field};\n    }}\n",
            ty = spec.type_name,
            field = spec.field_name,
        ));
    }
    out
}

/// Walk the source AST exactly like `compute_remaining_source_accessors`
/// but emit a TextEdit per access, rewriting the bare/qualified field name
/// (or its enclosing assignment / update_expression) through the delegate's
/// getter/setter. Skips entries we can't rewrite (e.g. write to a `final`
/// field — the underlying setter doesn't exist).
///
/// Two-pass design (Gap 27): an assignment of the form
/// `field = field.transform()` requires the LHS-write rewrite to consume the
/// WHOLE assignment (`field = ...` → `delegate.setField(...)`) while the RHS
/// reads inside the `...` still need to be rewritten through the getter.
/// A single-pass walk that emits LHS and RHS edits independently silently
/// drops one of them through the non-overlap guard. Pass 1 collects the set
/// of LHS-write sites; pass 2 emits all other access rewrites BUT skips any
/// access that lives inside one of those sites' RHS (those edits are folded
/// into the LHS write rewrite at the end). For each LHS-write site we then
/// materialize a single edit that spans the entire `assignment_expression`,
/// with replacement = `delegate.setX(<read-rewritten rhs text>)`.
fn compute_remaining_accessor_rewrite_edits(
    parsed: &ParsedSource,
    specs: &[DelegateAccessorSpec],
    moved_decl_ranges: &[(usize, usize)],
    delegate_field: &str,
    caller_edits: Vec<TextEdit>,
) -> Result<(Vec<TextEdit>, Vec<TextEdit>)> {
    let spec_by_name: HashMap<&str, &DelegateAccessorSpec> = specs
        .iter()
        .map(|spec| (spec.field_name.as_str(), spec))
        .collect();
    let name_set: HashSet<&str> = spec_by_name.keys().copied().collect();

    // ---------------- Pass 1: identify LHS-write sites ---------------------
    //
    // An LHS-write site is an `assignment_expression` whose `left` field
    // resolves to a moved field (bare identifier or `this.field`) AND for
    // which a setter is available. Each site is recorded with the full
    // assignment range, the RHS range, the setter name, and the operator
    // text — everything pass 2 needs to materialize the single combined edit.
    struct LhsWriteSite<'a> {
        assign: Node<'a>,
        rhs: Node<'a>,
        setter: String,
        op_text: Option<String>,
        getter_call: String,
    }
    let mut lhs_write_sites: Vec<LhsWriteSite> = Vec::new();
    {
        let mut stack = vec![parsed.tree.root_node()];
        while let Some(node) = stack.pop() {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                stack.push(child);
            }
            if node.kind() != "identifier" {
                continue;
            }
            let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
                continue;
            };
            if !name_set.contains(text) {
                continue;
            }
            if moved_decl_ranges
                .iter()
                .any(|(s, e)| node.start_byte() >= *s && node.end_byte() <= *e)
            {
                continue;
            }
            let access_node = match resolve_field_access(node) {
                Some(n) => n,
                None => continue,
            };
            if is_shadowed(node, text, &parsed.source) {
                continue;
            }
            let Some(spec) = spec_by_name.get(text).copied() else {
                continue;
            };
            // Walk past parenthesized/cast wrappers, mirroring classify.
            let mut classify_target = access_node;
            while let Some(parent) = classify_target.parent() {
                if matches!(
                    parent.kind(),
                    "parenthesized_expression" | "cast_expression"
                ) {
                    classify_target = parent;
                    continue;
                }
                break;
            }
            let Some(parent) = classify_target.parent() else {
                continue;
            };
            if parent.kind() != "assignment_expression" {
                continue;
            }
            let left_id = parent.child_by_field_name("left").map(|c| c.id());
            if left_id != Some(classify_target.id()) {
                continue;
            }
            let Some(setter) = spec.setter_name() else {
                // final field — no setter, so we can't fold this into a
                // write rewrite. Pass 2 will see the LHS access and silently
                // skip it (matching prior behavior on final-field writes).
                continue;
            };
            let Some(rhs) = parent.child_by_field_name("right") else {
                continue;
            };
            lhs_write_sites.push(LhsWriteSite {
                assign: parent,
                rhs,
                setter,
                op_text: assignment_operator_text(parent, &parsed.source),
                getter_call: format!("{delegate_field}.{}()", spec.getter_name()),
            });
        }
    }

    // Quick range-containment helper: is `(s,e)` strictly inside any LHS
    // site's RHS? Used by pass 2 to defer RHS edits into the per-site sub-
    // edit collection.
    let in_any_rhs = |start: usize, end: usize| -> Option<usize> {
        lhs_write_sites
            .iter()
            .position(|site| start >= site.rhs.start_byte() && end <= site.rhs.end_byte())
    };
    // The LHS access ranges themselves — pass 2 must skip them entirely;
    // they are consumed by the per-site write rewrite emitted at the end.
    let lhs_access_ranges: Vec<(usize, usize)> = lhs_write_sites
        .iter()
        .map(|site| {
            let left = site
                .assign
                .child_by_field_name("left")
                .unwrap_or(site.assign);
            (left.start_byte(), left.end_byte())
        })
        .collect();

    // ---------------- Pass 2: emit per-access edits ------------------------
    //
    // Each access falls into one of three buckets:
    //   (a) inside an LHS-write site's RHS  → record as a sub-edit on that
    //       site (will be applied to the RHS source text when we render the
    //       combined write rewrite).
    //   (b) IS the LHS access of an LHS-write site → skip (consumed by the
    //       combined write rewrite).
    //   (c) anywhere else → emit a normal edit into the global list.
    let mut edits: Vec<TextEdit> = Vec::new();
    let mut emitted_ranges: Vec<(usize, usize)> = Vec::new();
    // Per-site RHS sub-edits. Indexed by lhs_write_sites position. Sub-edits
    // are stored in absolute source-byte coordinates and translated to RHS-
    // local indices at render time.
    let mut rhs_sub_edits: Vec<Vec<TextEdit>> =
        (0..lhs_write_sites.len()).map(|_| Vec::new()).collect();

    // Gap 1: caller-rewrite absorption. `java_caller_rewrite_edits` emits
    // zero-width inserts at the start of `method_invocation` nodes (e.g.
    // `extractedGrid.` before `buildGrid()`). When an LHS-write of a moved
    // field has an RHS that contains such a call (`grid = buildGrid();`),
    // the LHS-write rewrite renders the WHOLE assignment as a single edit
    // spanning the assignment range — and the caller-rewrite zero-width
    // insert lands inside that span, tripping the planner's overlap
    // validator. Absorb every caller edit whose range is fully inside an
    // LHS-write RHS into that site's sub-edits, leaving the rest in the
    // residual list to return to the caller.
    let mut residual_caller_edits: Vec<TextEdit> = Vec::new();
    for edit in caller_edits {
        let absorbed = lhs_write_sites.iter().position(|site| {
            edit.byte_start >= site.rhs.start_byte() && edit.byte_end <= site.rhs.end_byte()
        });
        match absorbed {
            Some(site_idx) => {
                rhs_sub_edits[site_idx].push(edit);
            }
            None => residual_caller_edits.push(edit),
        }
    }

    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "identifier" {
            continue;
        }
        let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
            continue;
        };
        if !name_set.contains(text) {
            continue;
        }
        if moved_decl_ranges
            .iter()
            .any(|(s, e)| node.start_byte() >= *s && node.end_byte() <= *e)
        {
            continue;
        }
        let access_node = match resolve_field_access(node) {
            Some(n) => n,
            None => continue,
        };
        if is_shadowed(node, text, &parsed.source) {
            continue;
        }
        let Some(spec) = spec_by_name.get(text).copied() else {
            continue;
        };
        let getter_call = format!("{delegate_field}.{}()", spec.getter_name());

        // Walk past parenthesized/cast wrappers when classifying writes,
        // mirroring classify_access_kind.
        let mut classify_target = access_node;
        while let Some(parent) = classify_target.parent() {
            if matches!(
                parent.kind(),
                "parenthesized_expression" | "cast_expression"
            ) {
                classify_target = parent;
                continue;
            }
            break;
        }
        let parent = classify_target.parent();

        // Bucket (b): is this access THE LHS of an LHS-write site? Skip;
        // the combined write rewrite at the end consumes it.
        if lhs_access_ranges
            .iter()
            .any(|(s, e)| access_node.start_byte() == *s && access_node.end_byte() == *e)
        {
            continue;
        }

        // Compute the edit this access would produce in isolation
        // (start, end, replacement). For non-LHS-of-write assignments and
        // update_expressions the edit covers a wider span than the bare
        // identifier; for plain reads it covers just the access.
        let edit = match parent.map(|p| p.kind()) {
            Some("assignment_expression") => {
                let assign = parent.unwrap();
                let left_id = assign.child_by_field_name("left").map(|c| c.id());
                if left_id == Some(classify_target.id()) {
                    // LHS-of-write but no setter (handled above as a skip
                    // via lhs_access_ranges) — defensive: emit nothing.
                    None
                } else {
                    // RHS of an assignment — same as a read.
                    Some((
                        access_node.start_byte(),
                        access_node.end_byte(),
                        getter_call.clone(),
                    ))
                }
            }
            Some("update_expression") => {
                let upd = parent.unwrap();
                let setter = match spec.setter_name() {
                    Some(s) => s,
                    None => {
                        continue;
                    }
                };
                let op_text = update_operator_text(upd, &parsed.source);
                let bin_op = match op_text.as_deref() {
                    Some("++") => "+",
                    Some("--") => "-",
                    _ => {
                        continue;
                    }
                };
                Some((
                    upd.start_byte(),
                    upd.end_byte(),
                    format!("{delegate_field}.{setter}({getter_call} {bin_op} 1)"),
                ))
            }
            _ => Some((
                access_node.start_byte(),
                access_node.end_byte(),
                getter_call,
            )),
        };
        let Some((start, end, replacement)) = edit else {
            continue;
        };

        // Bucket (a): edit lives inside an LHS-write site's RHS — defer it
        // into the per-site sub-edits, do NOT add to the global edit list.
        if let Some(site_idx) = in_any_rhs(start, end) {
            // Defensive: if this edit happens to span outside the RHS
            // (shouldn't with current node kinds, but cheap to check),
            // fall through to the global path.
            let site = &lhs_write_sites[site_idx];
            if start >= site.rhs.start_byte() && end <= site.rhs.end_byte() {
                rhs_sub_edits[site_idx].push(TextEdit {
                    byte_start: start,
                    byte_end: end,
                    replacement,
                });
                continue;
            }
            // else fall through to the global push below.
        }

        // Bucket (c): non-overlap-guarded global edit list.
        if emitted_ranges
            .iter()
            .any(|(s, e)| ranges_overlap((*s, *e), (start, end)))
        {
            continue;
        }
        emitted_ranges.push((start, end));
        edits.push(TextEdit {
            byte_start: start,
            byte_end: end,
            replacement,
        });
    }

    // ---------------- Render LHS-write sites -------------------------------
    //
    // For each site, apply its accumulated RHS sub-edits to the original
    // RHS source text (last-to-first so byte indices stay valid), then
    // wrap the result in `delegate.setX(...)` (or compound form for `+=`,
    // etc.). Emit one edit covering the full assignment_expression range.
    for (site_idx, site) in lhs_write_sites.iter().enumerate() {
        let mut sub = rhs_sub_edits[site_idx].clone();
        sub.sort_by_key(|e| e.byte_start);
        // Reject sub-edits that overlap each other (defensive — shouldn't
        // happen, but be safe).
        let mut prev_end: Option<usize> = None;
        let mut clean_sub: Vec<TextEdit> = Vec::new();
        for e in sub {
            if let Some(pe) = prev_end {
                if e.byte_start < pe {
                    continue;
                }
            }
            prev_end = Some(e.byte_end);
            clean_sub.push(e);
        }
        let mut rhs_text = parsed.source[site.rhs.start_byte()..site.rhs.end_byte()].to_string();
        let rhs_base = site.rhs.start_byte();
        for e in clean_sub.iter().rev() {
            let local_start = e.byte_start - rhs_base;
            let local_end = e.byte_end - rhs_base;
            rhs_text.replace_range(local_start..local_end, &e.replacement);
        }
        let setter = &site.setter;
        let getter_call = &site.getter_call;
        let replacement = match site.op_text.as_deref() {
            Some("=") | None => {
                format!("{delegate_field}.{setter}({rhs_text})")
            }
            Some(compound) => {
                let bin_op = compound.trim_end_matches('=');
                format!("{delegate_field}.{setter}({getter_call} {bin_op} {rhs_text})")
            }
        };
        edits.push(TextEdit {
            byte_start: site.assign.start_byte(),
            byte_end: site.assign.end_byte(),
            replacement,
        });
    }

    // Gap 1: filter + validate against LHS-write containment leaks.
    //
    // The two-pass walk above catches RHS sub-edits (bucket a) and the LHS
    // access itself (bucket b), so in theory no leftover edit should land
    // strictly inside an LHS-write site's full assign span. In practice
    // zero-width inserts at the LHS position have slipped through (see
    // JAVA_TOOL_GAPS Gap 1: `33824..33880 overlaps 33854..33854`). Belt-
    // and-suspenders: drop any non-rendering edit whose range is fully
    // contained within an LHS-write span, then assert the invariant.
    let lhs_full_ranges: Vec<(usize, usize)> = lhs_write_sites
        .iter()
        .map(|site| (site.assign.start_byte(), site.assign.end_byte()))
        .collect();
    edits.retain(|edit| {
        // Keep the LHS-write rendering edits themselves (they ARE the
        // span). Drop any other edit whose range is fully contained.
        let is_rendering = lhs_full_ranges
            .iter()
            .any(|(s, e)| *s == edit.byte_start && *e == edit.byte_end);
        if is_rendering {
            return true;
        }
        !lhs_full_ranges
            .iter()
            .any(|(s, e)| edit.byte_start >= *s && edit.byte_end <= *e)
    });
    for edit in &edits {
        let is_rendering = lhs_full_ranges
            .iter()
            .any(|(s, e)| *s == edit.byte_start && *e == edit.byte_end);
        if is_rendering {
            continue;
        }
        if let Some((s, e)) = lhs_full_ranges
            .iter()
            .find(|(s, e)| edit.byte_start >= *s && edit.byte_end <= *e)
        {
            bail!(
                "internal: accessor rewrite emitted edit {}..{} contained within LHS-write span {}..{} \
                 (Gap 1 containment invariant violated; filter logic broken)",
                edit.byte_start,
                edit.byte_end,
                s,
                e
            );
        }
    }
    edits.sort_by_key(|e| e.byte_start);
    Ok((edits, residual_caller_edits))
}

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Return the operator token text of an `assignment_expression` (e.g. `=`,
/// `+=`, `<<=`, `>>>=`). tree-sitter-java exposes it as an unnamed child
/// between the `left` and `right` field children.
fn assignment_operator_text(assign: Node<'_>, source: &str) -> Option<String> {
    let left = assign.child_by_field_name("left")?;
    let right = assign.child_by_field_name("right")?;
    let mut cursor = assign.walk();
    let mut op = None;
    for child in assign.children(&mut cursor) {
        if child.id() == left.id() || child.id() == right.id() {
            continue;
        }
        if !child.is_named() {
            // unnamed child — likely the operator token.
            if let Ok(text) = child.utf8_text(source.as_bytes()) {
                op = Some(text.to_string());
            }
        }
    }
    op
}

/// Return the operator text of an `update_expression` — `++` or `--`.
fn update_operator_text(upd: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = upd.walk();
    for child in upd.children(&mut cursor) {
        if !child.is_named() {
            if let Ok(text) = child.utf8_text(source.as_bytes()) {
                if text == "++" || text == "--" {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

/// If `node` is an identifier, return the access expression we should
/// consider for kind classification: either the identifier itself (bare
/// access) or the enclosing `field_access` when the identifier sits in the
/// `field` position of a `this.<name>` or `(...).<name>` expression where
/// the object is `this`. Returns None when the identifier is not actually a
/// field-resolved access (method names, type names, declarators, accesses
/// on other objects, etc.).
#[allow(clippy::collapsible_match)]
fn resolve_field_access<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let parent = node.parent()?;
    let parent_kind = parent.kind();

    // Reject identifiers that are *names* of declarations or invocations.
    match parent_kind {
        "variable_declarator"
        | "formal_parameter"
        | "spread_parameter"
        | "catch_formal_parameter"
        | "method_declaration"
        | "constructor_declaration"
        | "class_declaration"
        | "interface_declaration"
        | "record_declaration"
        | "enum_declaration"
        | "annotation_type_declaration"
        | "labeled_statement"
        | "type_parameter"
        | "marker_annotation"
        | "annotation"
        | "enum_constant"
        | "method_invocation" => {
            if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) {
                return None;
            }
            // Otherwise (object position, etc.) it's a field/variable read.
        }
        "method_reference" => {
            // `Qualifier::name` — tree-sitter-java labels both qualifier-type
            // (`Foo`) and qualifier-field (`csvExtractors`) as identifiers.
            // Gap 3: when the qualifier is an instance field of the source
            // class, the capture analysis must see it so the field flows
            // through the target's constructor.
            //
            // Only the qualifier slot (first named child) is a candidate —
            // the method-name slot after `::` is never a field reference.
            // The caller does the field-name lookup against the
            // source-class field map plus shadowing, so type qualifiers
            // (uppercase identifiers) naturally fall out at lookup time
            // (they don't appear in the instance-field map).
            let mut cursor = parent.walk();
            let qualifier = parent.named_children(&mut cursor).next();
            if qualifier.map(|q| q.id()) != Some(node.id()) {
                return None;
            }
            // Fall through to `Some(node)` — let the caller apply the
            // field-map + shadowing checks. The pre-Gap-3 unconditional
            // return None here was the documented capture miss.
        }
        "field_access" => {
            // If this identifier is the `field` part of a field_access, then
            // the qualifying object decides whether this is *our* field.
            if parent.child_by_field_name("field").map(|c| c.id()) == Some(node.id()) {
                let object = parent.child_by_field_name("object")?;
                if object.kind() == "this" || object.kind() == "this_expression" {
                    return Some(parent);
                }
                // `something.fieldName` where `something` isn't `this` — not
                // our field.
                return None;
            }
            // Otherwise the identifier is in the `object` position — a bare
            // read of the field as the qualifier of a further access.
        }
        "scoped_identifier" | "scoped_type_identifier" | "type_identifier" | "generic_type" => {
            return None;
        }
        _ => {}
    }
    Some(node)
}

fn classify_access_kind(access_node: Node<'_>) -> &'static str {
    let mut current = access_node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "assignment_expression" => {
                if parent.child_by_field_name("left").map(|c| c.id()) == Some(current.id()) {
                    return "write";
                }
                return "read";
            }
            "update_expression" => {
                return "write";
            }
            "parenthesized_expression" | "cast_expression" => {
                current = parent;
                continue;
            }
            _ => return "read",
        }
    }
    "read"
}

fn is_shadowed(ident_node: Node<'_>, name: &str, source: &str) -> bool {
    let target_id = ident_node.id();
    let target_start = ident_node.start_byte();
    let mut current = ident_node;
    while let Some(parent) = current.parent() {
        let kind = parent.kind();
        if kind == "class_body" || kind == "interface_body" || kind == "enum_body" {
            return false;
        }
        if matches!(
            kind,
            "block"
                | "method_declaration"
                | "constructor_declaration"
                | "lambda_expression"
                | "for_statement"
                | "enhanced_for_statement"
                | "catch_clause"
                | "try_with_resources_statement"
                | "switch_block_statement_group"
                | "switch_block"
        ) {
            if scope_declares_name(parent, name, target_start, target_id, source) {
                return true;
            }
        }
        current = parent;
    }
    false
}

fn scope_declares_name(
    scope: Node<'_>,
    name: &str,
    target_start: usize,
    target_id: usize,
    source: &str,
) -> bool {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        // Don't descend into nested scopes (a local declared in a sibling
        // block doesn't shadow us).
        if node.id() != scope.id()
            && matches!(
                kind,
                "method_declaration"
                    | "constructor_declaration"
                    | "lambda_expression"
                    | "class_body"
                    | "interface_body"
                    | "enum_body"
                    | "class_declaration"
                    | "interface_declaration"
                    | "record_declaration"
                    | "enum_declaration"
            )
        {
            continue;
        }
        if matches!(
            kind,
            "local_variable_declaration"
                | "formal_parameter"
                | "spread_parameter"
                | "catch_formal_parameter"
                | "resource"
                | "enhanced_for_variable"
        ) {
            if declaration_name_matches(node, name, source) && node.start_byte() <= target_start {
                // Don't treat the target identifier itself as shadowing.
                if !node_contains_id(node, target_id) {
                    return true;
                }
            }
        }
        if kind == "lambda_expression" && node.id() != scope.id() {
            // Already handled above by skipping; keep here for clarity.
            continue;
        }
        if kind == "enhanced_for_statement" && node.id() != scope.id() {
            // `for (X x : ...)` declares `x` in the body's scope but the
            // declaration node lives at the for-statement level. Still need
            // to check it.
            if for_each_declares_name(node, name, source) && node.start_byte() <= target_start {
                if !node_contains_id(node, target_id) {
                    return true;
                }
            }
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn for_each_declares_name(for_node: Node<'_>, name: &str, source: &str) -> bool {
    if let Some(name_node) = for_node.child_by_field_name("name") {
        if let Ok(text) = name_node.utf8_text(source.as_bytes()) {
            return text == name;
        }
    }
    false
}

fn declaration_name_matches(node: Node<'_>, name: &str, source: &str) -> bool {
    match node.kind() {
        "local_variable_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if name_node.utf8_text(source.as_bytes()) == Ok(name) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        "formal_parameter" | "spread_parameter" | "catch_formal_parameter" | "resource" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                return name_node.utf8_text(source.as_bytes()) == Ok(name);
            }
            false
        }
        _ => false,
    }
}

fn node_contains_id(node: Node<'_>, target_id: usize) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.id() == target_id {
            return true;
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn surrounding_context(source: &str, idx: usize) -> String {
    let line_start = source[..idx.min(source.len())]
        .rfind('\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let after = &source[idx.min(source.len())..];
    let line_end_offset = after.find('\n').unwrap_or(after.len());
    let line_end = idx + line_end_offset;
    let line = source[line_start..line_end].trim();
    if line.chars().count() <= 80 {
        return line.to_string();
    }
    line.chars().take(77).collect::<String>() + "..."
}

fn java_caller_rewrite_edits(
    parsed: &ParsedSource,
    methods: &[String],
    delegate_field: &str,
    skip_ranges: &[(usize, usize)],
) -> Result<Vec<TextEdit>> {
    let methods = methods.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut edits = Vec::new();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let in_skip = skip_ranges
            .iter()
            .any(|(start, end)| node.start_byte() >= *start && node.end_byte() <= *end);
        if !in_skip {
            match node.kind() {
                "method_invocation" => {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                            if methods.contains(name) {
                                let prefix =
                                    parsed.source[node.start_byte()..name_node.start_byte()].trim();
                                if prefix.is_empty() {
                                    edits.push(TextEdit {
                                        byte_start: node.start_byte(),
                                        byte_end: node.start_byte(),
                                        replacement: format!("{delegate_field}."),
                                    });
                                } else if prefix == "this." {
                                    edits.push(TextEdit {
                                        byte_start: node.start_byte(),
                                        byte_end: name_node.start_byte(),
                                        replacement: format!("{delegate_field}."),
                                    });
                                }
                            }
                        }
                    }
                }
                "method_reference" => {
                    // tree-sitter-java emits a `method_reference` with two
                    // named children: the qualifier (a `this` keyword,
                    // `super` keyword, or type/expression node) and the
                    // method `identifier`. We rewrite only when the
                    // qualifier is `this` — `Foo::bar` and `super::method`
                    // bind to different receivers and must be left alone.
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.named_children(&mut cursor).collect();
                    if children.len() == 2 {
                        let qualifier = children[0];
                        let name_node = children[1];
                        if qualifier.kind() == "this" && name_node.kind() == "identifier" {
                            if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                                if methods.contains(name) {
                                    edits.push(TextEdit {
                                        byte_start: qualifier.start_byte(),
                                        byte_end: qualifier.end_byte(),
                                        replacement: delegate_field.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&edits)?;
    Ok(edits)
}

/// Gap 24: Render an extracted method as text destined for the target file,
/// widening its visibility modifier to at least `visibility_floor` so the
/// source-side delegate calls produced by `update_java_callers` can reach
/// it. Visibility ranks: private(0) < package(1) < protected(2) < public(3).
/// A method already at or above the floor is emitted unchanged. The floor
/// itself is `package` for same-package extractions and `public` when the
/// target ends up in a different package than the source.
/// Like `extract_method_text_with_visibility_floor` but for `field_declaration`
/// nodes — used to render moved static-final constants on the target with a
/// widened modifier when the constant's source-side visibility is below the
/// floor.
///
/// Same-package extracts use a `package` floor (no modifier — strip `private`);
/// cross-package extracts use `public`. Constants that already meet or exceed
/// the floor are emitted unchanged.
/// Return true when any of the extracted methods' bodies contains a write
/// (`assignment_expression` with LHS resolving to the field) or update
/// (`x++` / `x--`) of `field_name`. Used by the mutable-capture-with-write
/// refusal — captures that the moved code writes to cannot become `final`
/// constructor parameters.
/// Specification for a `callback_externals` entry. Built from a source-class
/// method declaration: tells the planner what functional-interface field to
/// put on the target, how to rewrite call sites inside the extracted bodies,
/// and what extra `import` to add to the target file.
#[derive(Debug, Clone)]
struct CallbackSpec {
    method_name: String,
    field_name: String,
    interface_type: String,
    invoke_method: String,
    extra_import: Option<String>,
}

/// Classify a source-class method declaration into a functional-interface
/// callback for use by `callback_externals`. Returns
/// `error.bad_input(code=callback_arity_unsupported)` when the method has
/// more than one parameter (BiConsumer / BiFunction support is future work).
fn classify_callback_method(parsed: &ParsedSource, method_node: Node<'_>) -> Result<CallbackSpec> {
    let method_name = method_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .ok_or_else(|| anyhow!("could not read callback method name"))?
        .to_string();
    let return_type_text = method_node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("void");
    let is_void = return_type_text == "void";
    let params_node = method_node
        .child_by_field_name("parameters")
        .ok_or_else(|| anyhow!("callback method `{method_name}` has no parameter list"))?;
    let mut cursor = params_node.walk();
    let formal_params: Vec<Node<'_>> = params_node
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "formal_parameter")
        .collect();
    let param_types: Vec<String> = formal_params
        .iter()
        .map(|p| {
            p.child_by_field_name("type")
                .and_then(|t| t.utf8_text(parsed.source.as_bytes()).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "?".to_string())
        })
        .collect();
    let (interface_type, invoke_method, extra_import) = match (param_types.len(), is_void) {
        (0, true) => ("Runnable".to_string(), "run".to_string(), None),
        (0, false) => (
            format!("Supplier<{}>", boxed_for_supplier(return_type_text)),
            "get".to_string(),
            Some("java.util.function.Supplier".to_string()),
        ),
        (1, true) => (
            format!("Consumer<{}>", boxed_for_supplier(&param_types[0])),
            "accept".to_string(),
            Some("java.util.function.Consumer".to_string()),
        ),
        (1, false) => (
            format!(
                "Function<{}, {}>",
                boxed_for_supplier(&param_types[0]),
                boxed_for_supplier(return_type_text)
            ),
            "apply".to_string(),
            Some("java.util.function.Function".to_string()),
        ),
        (n, _) => bail!(
            "error.bad_input(code=callback_arity_unsupported): callback method `{method_name}` \
             takes {n} parameters; only 0-arg and 1-arg signatures are supported (Runnable / \
             Supplier / Consumer / Function). Add a wrapper method or expand the planner's \
             callback support."
        ),
    };
    Ok(CallbackSpec {
        method_name: method_name.clone(),
        field_name: method_name,
        interface_type,
        invoke_method,
        extra_import,
    })
}

/// Build `(byte_start, byte_end, replacement)` edits over `target_text` that
/// rewrite every unqualified or `this.`-qualified invocation of a callback's
/// `method_name` to `<field_name>.<invoke_method>(args)`. Skips invocations
/// with non-`this` receivers (those bind to a different instance).
fn compute_callback_call_rewrites(
    target_text: &str,
    callbacks: &[CallbackSpec],
) -> Result<Vec<(usize, usize, String)>> {
    if callbacks.is_empty() {
        return Ok(Vec::new());
    }
    let tree = parse_source("java", target_text)?;
    let by_name: HashMap<&str, &CallbackSpec> = callbacks
        .iter()
        .map(|c| (c.method_name.as_str(), c))
        .collect();
    let mut edits = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if node.kind() != "method_invocation" {
            continue;
        }
        let Some(name_node) = node.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(target_text.as_bytes()) else {
            continue;
        };
        let Some(spec) = by_name.get(name) else {
            continue;
        };
        // Reject calls with a non-`this` receiver — they bind to a different
        // instance and should not be redirected through our callback.
        if let Some(object) = node.child_by_field_name("object") {
            if object.kind() != "this" && object.kind() != "this_expression" {
                continue;
            }
        }
        let args_node = node.child_by_field_name("arguments");
        let args_text = args_node
            .and_then(|n| n.utf8_text(target_text.as_bytes()).ok())
            .unwrap_or("()");
        // Strip the surrounding `(` `)` so we can rebuild a clean call.
        let args_inner = args_text
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim();
        let replacement = if args_inner.is_empty() {
            format!("{}.{}()", spec.field_name, spec.invoke_method)
        } else {
            format!("{}.{}({})", spec.field_name, spec.invoke_method, args_inner)
        };
        edits.push((node.start_byte(), node.end_byte(), replacement));
    }
    edits.sort_by_key(|(s, _, _)| *s);
    Ok(edits)
}

/// Apply `(start, end, replacement)` edits to `text` from rightmost to
/// leftmost so byte indices stay valid.
fn apply_text_edits(text: &str, edits: &[(usize, usize, String)]) -> String {
    let mut out = text.to_string();
    for (s, e, repl) in edits.iter().rev() {
        out.replace_range(*s..*e, repl);
    }
    out
}

fn extracted_methods_write_to(
    parsed: &ParsedSource,
    methods: &[JavaMethod],
    field_name: &str,
) -> bool {
    for method in methods {
        let Some(method_node) = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        }) else {
            continue;
        };
        let mut stack = vec![method_node];
        while let Some(node) = stack.pop() {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                stack.push(child);
            }
            if node.kind() != "identifier" {
                continue;
            }
            let Ok(text) = node.utf8_text(parsed.source.as_bytes()) else {
                continue;
            };
            if text != field_name {
                continue;
            }
            let Some(access_node) = resolve_field_access(node) else {
                continue;
            };
            if is_shadowed(node, text, &parsed.source) {
                continue;
            }
            // Walk past parenthesized/cast wrappers, then check whether the
            // access is on the LHS of an assignment or inside an update.
            let mut target = access_node;
            while let Some(parent) = target.parent() {
                if matches!(
                    parent.kind(),
                    "parenthesized_expression" | "cast_expression"
                ) {
                    target = parent;
                    continue;
                }
                break;
            }
            if let Some(parent) = target.parent() {
                match parent.kind() {
                    "assignment_expression"
                        if parent.child_by_field_name("left").map(|c| c.id()) == Some(target.id()) => {
                            return true;
                        }
                    "update_expression" => return true,
                    _ => {}
                }
            }
        }
    }
    false
}

fn extract_field_text_with_visibility_floor(
    parsed: &ParsedSource,
    field: &JavaField,
    visibility_floor: &str,
) -> String {
    let original = parsed.source[field.item.leading_trivia_start..field.item.byte_end]
        .trim_matches('\n')
        .to_string();
    let field_node = find_node(parsed.tree.root_node(), |node| {
        node.kind() == "field_declaration"
            && node.start_byte() == field.item.byte_start
            && node.end_byte() == field.item.byte_end
    });
    let Some(field_node) = field_node else {
        return original;
    };
    let mods = collect_java_modifiers(field_node);
    let current = java_visibility_from_mods(&mods);
    if java_visibility_rank(current) >= java_visibility_rank(visibility_floor) {
        return original;
    }
    let new_visibility = if visibility_floor == "package" {
        None
    } else {
        Some(visibility_floor)
    };
    let edit = build_visibility_rewrite_edit(field_node, &mods, new_visibility, &parsed.source);
    let window_start = field.item.leading_trivia_start;
    let window_end = field.item.byte_end;
    if edit.byte_start < window_start || edit.byte_end > window_end {
        return original;
    }
    let local_start = edit.byte_start - window_start;
    let local_end = edit.byte_end - window_start;
    let raw = &parsed.source[window_start..window_end];
    let mut rewritten = String::with_capacity(raw.len() + edit.replacement.len());
    rewritten.push_str(&raw[..local_start]);
    rewritten.push_str(&edit.replacement);
    rewritten.push_str(&raw[local_end..]);
    rewritten.trim_matches('\n').to_string()
}

fn extract_method_text_with_visibility_floor(
    parsed: &ParsedSource,
    method: &JavaMethod,
    visibility_floor: &str,
) -> String {
    let original = parsed.source[method.item.leading_trivia_start..method.item.byte_end]
        .trim_matches('\n')
        .to_string();
    let method_node = find_node(parsed.tree.root_node(), |node| {
        (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
            && node.start_byte() == method.item.byte_start
            && node.end_byte() == method.item.byte_end
    });
    let Some(method_node) = method_node else {
        return original;
    };
    let mods = collect_java_modifiers(method_node);
    let current = java_visibility_from_mods(&mods);
    if java_visibility_rank(current) >= java_visibility_rank(visibility_floor) {
        return original;
    }
    let new_visibility = if visibility_floor == "package" {
        None
    } else {
        Some(visibility_floor)
    };
    let edit = build_visibility_rewrite_edit(method_node, &mods, new_visibility, &parsed.source);
    // Edits are in absolute source coordinates. Re-base into the
    // leading_trivia_start..byte_end window we sliced for `original`.
    let window_start = method.item.leading_trivia_start;
    let window_end = method.item.byte_end;
    if edit.byte_start < window_start || edit.byte_end > window_end {
        return original;
    }
    let local_start = edit.byte_start - window_start;
    let local_end = edit.byte_end - window_start;
    let raw = &parsed.source[window_start..window_end];
    let mut rewritten = String::with_capacity(raw.len() + edit.replacement.len());
    rewritten.push_str(&raw[..local_start]);
    rewritten.push_str(&edit.replacement);
    rewritten.push_str(&raw[local_end..]);
    rewritten.trim_matches('\n').to_string()
}

fn java_visibility_from_mods(mods: &[(String, usize, usize)]) -> &str {
    for (mod_name, _, _) in mods {
        match mod_name.as_str() {
            "public" => return "public",
            "protected" => return "protected",
            "private" => return "private",
            _ => continue,
        }
    }
    "package"
}

fn build_visibility_rewrite_edit(
    method_node: Node<'_>,
    current_mods: &[(String, usize, usize)],
    new_visibility: Option<&str>,
    source: &str,
) -> TextEdit {
    let vis_mods: Vec<&(String, usize, usize)> = current_mods
        .iter()
        .filter(|(name, _, _)| matches!(name.as_str(), "public" | "protected" | "private"))
        .collect();

    match vis_mods.as_slice() {
        [] => {
            let target = match new_visibility {
                Some(v) => format!("{v} "),
                None => String::new(),
            };
            let non_vis_mods: Vec<&(String, usize, usize)> = current_mods
                .iter()
                .filter(|(name, _, _)| !matches!(name.as_str(), "public" | "protected" | "private"))
                .collect();
            if let Some(first_mod) = non_vis_mods.first() {
                TextEdit {
                    byte_start: first_mod.1,
                    byte_end: first_mod.1,
                    replacement: target,
                }
            } else {
                let ret_type = method_node
                    .child_by_field_name("return_type")
                    .or_else(|| method_node.child_by_field_name("type"));
                if let Some(rt) = ret_type {
                    TextEdit {
                        byte_start: rt.start_byte(),
                        byte_end: rt.start_byte(),
                        replacement: target,
                    }
                } else {
                    TextEdit {
                        byte_start: method_node.start_byte(),
                        byte_end: method_node.start_byte(),
                        replacement: target,
                    }
                }
            }
        }
        [vis] => {
            if let Some(new_vis) = new_visibility {
                TextEdit {
                    byte_start: vis.1,
                    byte_end: vis.2,
                    replacement: new_vis.to_string(),
                }
            } else {
                let vis_end = vis.2;
                let next_byte = source.as_bytes().get(vis_end);
                let end = if next_byte == Some(&b' ') || next_byte == Some(&b'\t') {
                    vis_end + 1
                } else {
                    vis_end
                };
                TextEdit {
                    byte_start: vis.1,
                    byte_end: end,
                    replacement: String::new(),
                }
            }
        }
        _ => {
            let first = vis_mods[0];
            let last = vis_mods[vis_mods.len() - 1];
            let replacement = new_visibility.unwrap_or("").to_string();
            let next_byte = source.as_bytes().get(last.2);
            let end = if next_byte == Some(&b' ') || next_byte == Some(&b'\t') {
                last.2 + 1
            } else {
                last.2
            };
            TextEdit {
                byte_start: first.1,
                byte_end: end,
                replacement,
            }
        }
    }
}

use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionParams, Position, Range, TextDocumentIdentifier,
    request::CodeActionRequest,
};

use crate::lsp::{LspError, LspSessionManager};
use crate::projects::Language;
pub(crate) use atom_plans::{
    plan_add_java_constructor, plan_add_java_delegate_field, plan_add_java_fields,
    plan_extract_java_nested_classes, plan_move_java_field, plan_rewrite_java_visibility,
};
pub(crate) use collapse_chain::plan_java_collapse_call_chain;
use cross_file::{MovedStaticItem, compute_cross_file_static_caller_edits};
pub(crate) use extract_class::plan_extract_java_class;
pub(crate) use extract_code_block::plan_extract_java_code_block_to_method;
pub(crate) use extract_methods::plan_extract_java_methods;
pub(crate) use inline_method::plan_inline_java_method;
pub(crate) use leaf_plans::{
    plan_add_java_implements, plan_extract_java_interface, plan_java_lsp_organize_imports,
    plan_migrate_java_type_usages,
};
pub(crate) use lombokify::plan_lombokify_java_class;
pub(crate) use method_object::plan_convert_method_to_class;
pub(crate) use migrate_receiver::plan_migrate_java_method_receiver;
pub(crate) use move_and_callers::{plan_move_java_constant, plan_update_java_callers};
pub(crate) use promote_inner::plan_promote_java_inner_class;
pub(crate) use prune_orphans::plan_prune_java_orphans;
pub(crate) use replace_static_ref::plan_replace_java_static_reference;
pub(crate) use singletonify::{plan_singletonify_java_holder, plan_singletonify_java_util};
pub(crate) use split_provider::plan_java_split_provider;
pub(crate) use test_slice::plan_extract_java_test_slice;

mod atom_plans;
mod collapse_chain;
mod di_plumbing;
mod extract_class;
mod extract_code_block;
mod extract_methods;
mod find_usages;
mod inline_method;
mod leaf_plans;
mod lombokify;
mod method_object;
mod migrate_receiver;
mod move_and_callers;
mod promote_inner;
mod prune_orphans;
mod replace_static_ref;
mod scope;
mod singletonify;
mod split_provider;
mod test_slice;
#[cfg(test)]
mod tests;
pub(crate) use find_usages::plan_find_java_usages;
mod rename_symbol;
pub(crate) use rename_symbol::plan_rename_java_symbol;
mod class_dependency;
pub(crate) use class_dependency::plan_java_class_dependency_analysis;
mod imports;
mod public_api_guard;
use imports::*;
pub(crate) use public_api_guard::plan_java_public_api_guard;

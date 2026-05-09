use super::*;
use std::collections::{BTreeMap, HashSet};

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

fn extract_java_package(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("package ") {
            let pkg = trimmed
                .strip_prefix("package ")?
                .trim_end_matches(';')
                .trim()
                .to_string();
            return Some(pkg);
        }
        if !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with("*")
        {
            break;
        }
    }
    None
}

fn extract_java_imports(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("import ") && line.ends_with(';'))
        .map(str::to_string)
        .collect()
}

fn java_import_simple_name(import_line: &str) -> Option<String> {
    let body = import_line
        .trim()
        .strip_prefix("import ")?
        .trim_end_matches(';')
        .trim();
    if body.starts_with("static ") || body.ends_with(".*") {
        return None;
    }
    body.rsplit('.').next().map(str::to_string)
}

fn java_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "String"
            | "Object"
            | "Class"
            | "Integer"
            | "Long"
            | "Short"
            | "Byte"
            | "Float"
            | "Double"
            | "Boolean"
            | "Character"
            | "Number"
            | "Void"
            | "Exception"
            | "RuntimeException"
            | "Throwable"
            | "Error"
            | "Override"
            | "Deprecated"
            | "SuppressWarnings"
    )
}

fn collect_java_type_references(node: Node<'_>, source: &str, out: &mut HashSet<String>) {
    if node.kind() == "type_identifier" {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            if text.chars().next().is_some_and(|c| c.is_uppercase()) && !java_builtin_type(text) {
                out.insert(text.to_string());
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_type_references(child, source, out);
    }
}

fn java_import_block_range(source: &str) -> (usize, usize, usize) {
    let mut offset = 0usize;
    let mut package_end = 0usize;
    let mut first_import = None;
    let mut last_import_end = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let line_end = offset + line.len();
        if trimmed.starts_with("package ") {
            package_end = line_end;
        }
        if trimmed.starts_with("import ") {
            first_import.get_or_insert(offset);
            last_import_end = Some(line_end);
        }
        offset = line_end;
    }
    let insert_at = if package_end > 0 { package_end } else { 0 };
    (
        first_import.unwrap_or(insert_at),
        last_import_end.unwrap_or(insert_at),
        insert_at,
    )
}

fn project_java_type_index(project_dir: &Path) -> Result<BTreeMap<String, Option<String>>> {
    let JavaTypeIndex { top_level, .. } = build_java_type_index(project_dir)?;
    Ok(top_level)
}

#[derive(Default, Debug)]
struct JavaTypeIndex {
    /// Simple-name → uniquely-resolvable FQCN for *top-level* types,
    /// or `None` when the simple name is ambiguous across packages.
    /// Mirrors the historical `project_java_type_index` shape.
    top_level: BTreeMap<String, Option<String>>,
    /// Simple names of inner classes (members of class/interface/
    /// record/enum bodies) discovered in the project. Inner-class
    /// references must be left in qualified form (`Outer.Inner`)
    /// rather than imported as a bare simple name; gap 16 lives
    /// here.
    inner_class_names: HashSet<String>,
}

fn build_java_type_index(project_dir: &Path) -> Result<JavaTypeIndex> {
    let mut idx = JavaTypeIndex::default();
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
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let Some(package) = extract_java_package(&source) else {
            continue;
        };
        let fqcn = format!("{package}.{simple}");
        match idx.top_level.get_mut(simple) {
            Some(slot) => *slot = None,
            None => {
                idx.top_level.insert(simple.to_string(), Some(fqcn));
            }
        }
        // Best-effort inner-class scan via tree-sitter. Skip on
        // parse failure so a single malformed file doesn't poison
        // the whole index.
        if let Ok(parsed) = parse_source_file(path) {
            collect_inner_class_simple_names(
                parsed.tree.root_node(),
                &parsed.source,
                false,
                &mut idx.inner_class_names,
            );
        }
    }
    Ok(idx)
}

/// Walk a Java parse tree adding nested class/interface/record/enum
/// names to `out`. `inside_type_body` indicates whether the current
/// node is contained in a type body (i.e. would yield an inner
/// class on definition). Top-level types are skipped — gap 16 only
/// cares about nested ones.
fn collect_inner_class_simple_names(
    node: Node<'_>,
    source: &str,
    inside_type_body: bool,
    out: &mut HashSet<String>,
) {
    let kind = node.kind();
    let is_type_decl = matches!(
        kind,
        "class_declaration" | "interface_declaration" | "record_declaration" | "enum_declaration"
    );
    if is_type_decl && inside_type_body {
        if let Some(name_node) = node.child_by_field_name("name") {
            if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                out.insert(name.to_string());
            }
        }
    }
    let next_inside = inside_type_body
        || matches!(kind, "class_body" | "enum_body" | "record_body" | "interface_body");
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_inner_class_simple_names(child, source, next_inside, out);
    }
}

fn heuristic_java_organize_imports(
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<FileEdit>> {
    let parsed = parse_source_file(source_path)?;
    if parsed.language != "java" {
        bail!("java_lsp_organize_imports fallback only supports java files");
    }
    let mut used_types = HashSet::new();
    collect_java_type_references(parsed.tree.root_node(), &parsed.source, &mut used_types);
    let current_package = extract_java_package(&parsed.source);
    let existing_imports = extract_java_imports(&parsed.source);
    let mut imports = existing_imports
        .iter()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("import static ") || trimmed.ends_with(".*;") {
                return true;
            }
            java_import_simple_name(trimmed)
                .is_some_and(|simple| used_types.contains(simple.as_str()))
        })
        .cloned()
        .collect::<HashSet<_>>();

    let JavaTypeIndex {
        top_level: known_types,
        inner_class_names,
    } = build_java_type_index(project_dir)?;
    let existing_simple = imports
        .iter()
        .filter_map(|line| java_import_simple_name(line))
        .collect::<HashSet<_>>();
    for used in &used_types {
        if existing_simple.contains(used) {
            continue;
        }
        // Gap 16: if this simple name corresponds to an inner class
        // in the project but has no uniquely-resolvable top-level
        // type, the reference must already be in qualified form
        // (`Outer.Inner`). Skip generating any bare-name import for
        // it — that would either fail to compile or silently bind
        // to the wrong package's `Inner`.
        let top_level_unique = matches!(known_types.get(used), Some(Some(_)));
        if !top_level_unique && inner_class_names.contains(used) {
            continue;
        }
        let Some(Some(fqcn)) = known_types.get(used) else {
            continue;
        };
        if current_package
            .as_deref()
            .is_some_and(|pkg| fqcn.strip_suffix(&format!(".{used}")) == Some(pkg))
        {
            continue;
        }
        imports.insert(format!("import {fqcn};"));
    }

    let mut sorted = imports.into_iter().collect::<Vec<_>>();
    sorted.sort();
    let (start, end, insert_at) = java_import_block_range(&parsed.source);
    let replacement = if sorted.is_empty() {
        String::new()
    } else if start == end && insert_at == 0 {
        format!("{}\n\n", sorted.join("\n"))
    } else if start == end {
        format!("\n{}\n", sorted.join("\n"))
    } else {
        sorted.join("\n")
    };
    if parsed.source[start..end] == replacement {
        return Ok(Vec::new());
    }
    Ok(vec![FileEdit {
        path: path_string(source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: vec![TextEdit {
            byte_start: start,
            byte_end: end,
            replacement,
        }],
    }])
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

fn java_default_target_prelude(p: &RefactorPlanParams, source: &str) -> String {
    if let Some(prelude) = p.target_prelude.as_deref() {
        let trimmed = prelude.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        return format!("{trimmed}\n\n");
    }
    extract_java_package(source)
        .map(|pkg| {
            let imports = extract_java_imports(source);
            if imports.is_empty() {
                format!("package {pkg};\n\n")
            } else {
                format!("package {pkg};\n\n{}\n\n", imports.join("\n"))
            }
        })
        .unwrap_or_else(|| {
            let imports = extract_java_imports(source);
            if imports.is_empty() {
                String::new()
            } else {
                format!("{}\n\n", imports.join("\n"))
            }
        })
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
                fields.push(JavaField {
                    name,
                    type_name,
                    item: syntax_item_with_kind(parsed, node, "field_declaration"),
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

fn collect_identifier_texts(node: Node<'_>, source: &str, out: &mut HashSet<String>) {
    if node.kind() == "identifier" {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            out.insert(text.to_string());
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_identifier_texts(child, source, out);
    }
}

fn captured_fields_for_methods(
    parsed: &ParsedSource,
    selected: &[JavaMethod],
) -> Vec<CapturedVariable> {
    let fields = java_fields(parsed)
        .into_iter()
        .map(|field| (field.name.clone(), field))
        .collect::<BTreeMap<_, _>>();
    let mut seen = HashSet::new();
    for method in selected {
        if let Some(node) = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        }) {
            collect_identifier_texts(node, &parsed.source, &mut seen);
        }
    }
    fields
        .into_iter()
        .filter(|(name, _)| seen.contains(name))
        .map(|(name, field)| {
            let field_node = find_node(parsed.tree.root_node(), |node| {
                node.kind() == "field_declaration"
                    && node.start_byte() == field.item.byte_start
                    && node.end_byte() == field.item.byte_end
            });
            let mods = field_node
                .map(collect_java_modifiers)
                .unwrap_or_default();
            let has_static = mods.iter().any(|(name, _, _)| name == "static");
            let has_final = mods.iter().any(|(name, _, _)| name == "final");
            CapturedVariable {
                name,
                kind: "field".to_string(),
                source_type: field.type_name,
                source_visibility: java_modifier_text(
                    field_node.unwrap_or(parsed.tree.root_node()),
                    &parsed.source,
                ),
                source_static_final: has_static && has_final,
                source_mutable: !has_final,
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
    let body = type_node
        .child_by_field_name("body")
        .unwrap_or(type_node);
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
struct InvocationHit<'a> {
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
fn collect_method_invocations<'a>(
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
                let receiver_text = obj
                    .utf8_text(parsed.source.as_bytes())
                    .unwrap_or("")
                    .trim();
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
fn java_source_class_method_signatures(
    parsed: &ParsedSource,
    class_node: Node<'_>,
) -> BTreeMap<String, (String, bool)> {
    let mut out: BTreeMap<String, (String, bool)> = BTreeMap::new();
    let body = class_node
        .child_by_field_name("body")
        .unwrap_or(class_node);
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "method_declaration" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                    let (sig, partial) = java_method_signature_text(child, &parsed.source);
                    // First definition wins; overloads collapse to one entry
                    // since we can't distinguish them at the call site without
                    // full type resolution.
                    out.entry(name.to_string()).or_insert((sig, partial));
                }
            }
        }
    }
    out
}

/// Walk `extends` / `implements` chains starting from a source class to
/// build a map of inherited method name -> (declaring type name, kind).
/// The first declaration found along BFS wins, mirroring Java's nearest-
/// ancestor resolution. Cycles are guarded via a visited set.
fn collect_inherited_method_declarations(
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
        let context = if hit.inside_lambda { "lambda" } else { "direct" };
        let site = ExtractedCallSite {
            line: hit.line,
            column: hit.column,
            in_method: hit.in_method.clone(),
            context: context.to_string(),
        };
        if let Some((sig, partial)) = source_methods.get(&hit.name) {
            // External: declared on source class body.
            let entry = external
                .entry(hit.name.clone())
                .or_insert_with(|| ExternalCall {
                    method: hit.name.clone(),
                    signature: sig.clone(),
                    signature_partial: *partial,
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
        let matches = candidates
            .iter()
            .filter(|m| m.item.name.as_deref() == Some(expected.as_str()))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested method `{expected}` was not found"),
            [method] => selected.push((**method).clone()),
            _ => bail!(
                "requested method `{expected}` matched multiple methods; method overloading requires more specific targeting (not yet implemented)"
            ),
        }
    }
    Ok(selected)
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

pub(crate) fn plan_extract_java_methods(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_methods"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_methods only supports java files");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    let mut selected = select_java_methods_by_name(&parsed, names)?;
    let captured_variables = captured_fields_for_methods(&parsed, &selected);
    let dependency_report = if p.deep_analysis.unwrap_or(false) {
        analyze_extracted_dependencies(
            &parsed,
            &selected,
            p.project_dir.as_deref().map(Path::new),
        )
    } else {
        Default::default()
    };

    selected.sort_by_key(|m| std::cmp::Reverse(m.item.byte_start));

    let mut source_edits = Vec::new();
    let mut extracted_content = Vec::new();

    for method in &selected {
        source_edits.push(TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.byte_end,
            replacement: String::new(),
        });
        let content = &parsed.source[method.item.leading_trivia_start..method.item.byte_end];
        extracted_content.push(content.to_string());
    }

    extracted_content.reverse();

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let target_content = if target_path.exists() {
        let mut text = String::from_utf8(original_target_bytes.clone()).unwrap_or_default();
        let insert_at = text.rfind('}').unwrap_or(text.len());
        text.insert_str(
            insert_at,
            &format!("\n{}\n", extracted_content.join("\n\n")),
        );
        text
    } else {
        let class_name = java_target_type_name(p, &target_path)?;
        let prelude = java_default_target_prelude(p, &parsed.source);
        java_class_wrapper(&class_name, &prelude, &extracted_content.join("\n\n"))
    };

    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract {} methods to {}",
            selected.len(),
            target_path.display()
        ),
        kind: "extract_java_methods".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            target_edit,
        ],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&source_path),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&target_path),
                byte_range: None,
            },
        ],
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables,
        remaining_source_accessors: Vec::new(),
        external_calls: dependency_report.external_calls,
        inherited_dependencies: dependency_report.inherited_dependencies,
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_java_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_class"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_class only supports java files");
    }
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?;
    let target_class_name = java_target_type_name(p, &target_path)?;
    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required for extract_java_class"))?;
    validate_java_member_name(delegate_field, "delegate_field")?;

    let method_names = p.item_names.as_deref().unwrap_or_default();
    let mut selected_methods = select_java_methods_by_name(&parsed, method_names)?;
    let moved_field_names = p.move_fields.as_deref().unwrap_or_default();
    let selected_fields = select_java_fields_by_name(&parsed, moved_field_names)?;
    let moved_field_set = selected_fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<HashSet<_>>();
    let captured_variables = captured_fields_for_methods(&parsed, &selected_methods);
    let class_dependency_report = if p.deep_analysis.unwrap_or(false) {
        analyze_extracted_dependencies(
            &parsed,
            &selected_methods,
            p.project_dir.as_deref().map(Path::new),
        )
    } else {
        Default::default()
    };

    // Gap 20: split captures into static-final constants vs instance
    // captures. Constants are moved declaration-and-initializer onto the
    // target (mirroring move_java_constant semantics), instance captures
    // become constructor parameters.
    let static_final_capture_names: HashSet<&str> = captured_variables
        .iter()
        .filter(|capture| capture.source_static_final)
        .filter(|capture| !moved_field_set.contains(capture.name.as_str()))
        .map(|capture| capture.name.as_str())
        .collect();
    let dependency_params = captured_variables
        .iter()
        .filter(|capture| !moved_field_set.contains(capture.name.as_str()))
        .filter(|capture| !static_final_capture_names.contains(capture.name.as_str()))
        .map(|capture| JavaParameterSpec {
            type_name: capture.source_type.clone(),
            name: capture.name.clone(),
        })
        .collect::<Vec<_>>();

    // Locate the source-side `field_declaration` node for each static-final
    // capture so we can (a) render the original declaration verbatim onto
    // the target and (b) emit a removal edit on the source.
    let mut moved_constant_fields: Vec<JavaField> = Vec::new();
    for name in static_final_capture_names.iter() {
        let field = java_fields(&parsed)
            .into_iter()
            .find(|field| field.name == *name);
        if let Some(field) = field {
            moved_constant_fields.push(field);
        }
    }
    moved_constant_fields.sort_by_key(|field| field.item.byte_start);

    // Gap 24: decide the visibility floor for delegate-rewritten methods.
    // Default floor is `package` (`update_java_callers` rewrites local calls
    // through the delegate, which only requires package-visible access);
    // if the target ends up in a different package than the source, the
    // floor escalates to `public`.
    let source_package = extract_java_package(&parsed.source);
    let target_existing_package = if target_path.exists() {
        fs::read_to_string(&target_path)
            .ok()
            .and_then(|content| extract_java_package(&content))
    } else {
        None
    };
    let target_package_from_prelude = p
        .target_prelude
        .as_deref()
        .and_then(extract_java_package);
    // Target package resolution mirrors java_default_target_prelude:
    // 1. explicit target_prelude wins, 2. existing target file's package,
    // 3. fallback to the source's package (the prelude inheritance path).
    let target_package = target_package_from_prelude
        .or(target_existing_package)
        .or_else(|| source_package.clone());
    let cross_package = match (source_package.as_deref(), target_package.as_deref()) {
        (Some(src), Some(tgt)) => src != tgt,
        _ => false,
    };
    let mut visibility_floor = if cross_package { "public" } else { "package" };
    if let Some(requested) = p.visibility.as_deref() {
        validate_java_visibility(requested)?;
        if java_visibility_rank(requested) > java_visibility_rank(visibility_floor) {
            visibility_floor = match requested {
                "public" => "public",
                "protected" => "protected",
                "package" => "package",
                "private" => "private",
                _ => visibility_floor,
            };
        }
    }

    selected_methods.sort_by_key(|method| method.item.byte_start);
    let method_text = selected_methods
        .iter()
        .map(|method| {
            extract_method_text_with_visibility_floor(
                &parsed,
                method,
                visibility_floor,
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let moved_field_text = selected_fields
        .iter()
        .map(|field| {
            parsed.source[field.item.leading_trivia_start..field.item.byte_end]
                .trim_matches('\n')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let moved_constants_text = moved_constant_fields
        .iter()
        .map(|field| {
            parsed.source[field.item.leading_trivia_start..field.item.byte_end]
                .trim_matches('\n')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dependency_field_text = dependency_params
        .iter()
        .map(|param| format!("    private final {} {};", param.type_name, param.name))
        .collect::<Vec<_>>()
        .join("\n");
    let constructor_text = if dependency_params.is_empty() {
        String::new()
    } else {
        java_constructor_decl(&target_class_name, "public", &dependency_params, true, None)?
    };
    let target_body = [
        moved_constants_text,
        dependency_field_text,
        moved_field_text,
        constructor_text,
        method_text,
    ]
    .into_iter()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let prelude = java_default_target_prelude(p, &parsed.source);
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    if !original_target_bytes.is_empty() {
        bail!("extract_java_class currently requires a missing or empty target file");
    }
    let target_content = java_class_wrapper(&target_class_name, &prelude, &target_body);

    let mut source_edits = Vec::new();
    let removed_ranges = selected_methods
        .iter()
        .map(|method| (method.item.leading_trivia_start, method.item.byte_end))
        .chain(
            selected_fields
                .iter()
                .map(|field| (field.item.leading_trivia_start, field.item.byte_end)),
        )
        .chain(
            moved_constant_fields
                .iter()
                .map(|field| (field.item.leading_trivia_start, field.item.byte_end)),
        )
        .collect::<Vec<_>>();
    for (start, end) in &removed_ranges {
        source_edits.push(TextEdit {
            byte_start: *start,
            byte_end: *end,
            replacement: String::new(),
        });
    }
    let field_insert_at = java_class_body_insert_position(class_node, &parsed.source);
    let delegate_edit_idx = source_edits.len();
    source_edits.push(TextEdit {
        byte_start: field_insert_at,
        byte_end: field_insert_at,
        replacement: format!("\n    private final {target_class_name} {delegate_field};"),
    });
    let assignment = format!(
        "this.{delegate_field} = new {target_class_name}({});",
        dependency_params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(constructor) = first_constructor_node(class_node, &parsed.source) {
        let insert_at = constructor_body_insert_position(constructor, &parsed.source);
        source_edits.push(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement: format!("\n        {assignment}"),
        });
    } else {
        let class_name = java_class_name(class_node, &parsed.source);
        let constructor =
            java_constructor_decl(&class_name, "public", &[], false, Some(&assignment))?;
        source_edits[delegate_edit_idx].replacement = format!(
            "\n    private final {target_class_name} {delegate_field};\n\n{}",
            constructor.trim_end()
        );
    }
    source_edits.extend(java_caller_rewrite_edits(
        &parsed,
        method_names,
        delegate_field,
        &removed_ranges,
    )?);
    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "Extract Java class {} from {}",
            target_class_name,
            source_path.display()
        ),
        kind: "extract_java_class".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(&original_target_bytes),
                edits: vec![TextEdit {
                    byte_start: 0,
                    byte_end: original_target_bytes.len(),
                    replacement: target_content,
                }],
            },
        ],
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: selected_methods
            .into_iter()
            .map(|method| method.item)
            .chain(selected_fields.into_iter().map(|field| field.item))
            .collect(),
        leftovers: Vec::new(),
        captured_variables,
        remaining_source_accessors: Vec::new(),
        external_calls: class_dependency_report.external_calls,
        inherited_dependencies: class_dependency_report.inherited_dependencies,
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_java_nested_classes(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_nested_classes"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_nested_classes only supports java files");
    }

    let candidates = java_nested_classes(&parsed);
    if candidates.is_empty() {
        bail!("no Java nested classes found");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    if names.is_empty() {
        bail!("item_names (class names) must be provided for extract_java_nested_classes");
    }

    let mut selected: Vec<JavaNestedClass> = Vec::new();
    for expected in names {
        let matches = candidates
            .iter()
            .filter(|c| c.item.name.as_deref() == Some(expected))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested nested class `{expected}` was not found"),
            [class_item] => selected.push((**class_item).clone()),
            _ => bail!("requested nested class `{expected}` matched multiple classes"),
        }
    }

    selected.sort_by_key(|c| std::cmp::Reverse(c.item.byte_start));

    let mut source_edits = Vec::new();
    let mut extracted_content = Vec::new();

    for class_item in &selected {
        source_edits.push(TextEdit {
            byte_start: class_item.item.leading_trivia_start,
            byte_end: class_item.item.byte_end,
            replacement: String::new(),
        });
        let content =
            &parsed.source[class_item.item.leading_trivia_start..class_item.item.byte_end];
        extracted_content.push(content.to_string());
    }

    extracted_content.reverse();

    let prelude = p.target_prelude.clone().unwrap_or_default();
    let target_content = format!("{}\n\n{}\n", prelude, extracted_content.join("\n\n"));

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract {} nested classes to {}",
            selected.len(),
            target_path.display()
        ),
        kind: "extract_java_nested_classes".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            target_edit,
        ],
        validations: vec![],
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_add_java_fields(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("add_java_fields only supports java files");
    }
    let fields = p
        .fields
        .as_deref()
        .filter(|fields| !fields.is_empty())
        .ok_or_else(|| anyhow!("fields is required for add_java_fields"))?;

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

    let existing = java_fields(&parsed)
        .into_iter()
        .map(|field| field.name)
        .collect::<HashSet<_>>();
    let mut declarations = String::new();
    for field in fields {
        if existing.contains(&field.name) {
            continue;
        }
        declarations.push_str(&java_field_decl(field)?);
    }
    if declarations.is_empty() {
        bail!("all requested Java fields already exist");
    }

    let insert_at = java_after_fields_insert_position(class_node, &parsed.source);
    let replacement = if insert_at == java_class_body_insert_position(class_node, &parsed.source) {
        format!("\n{}", declarations.trim_end())
    } else {
        format!("\n{}", declarations.trim_end())
    };
    let plan = RefactorPlan {
        title: format!(
            "Add {} Java field(s) to {}",
            fields.len(),
            source_path.display()
        ),
        kind: "add_java_fields".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_add_java_constructor(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("add_java_constructor only supports java files");
    }
    let params = p.parameters.as_deref().unwrap_or_default();
    let visibility = p.visibility.as_deref().unwrap_or("public");
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
    let class_name = java_class_name(class_node, &parsed.source);
    let constructor = java_constructor_decl(
        &class_name,
        visibility,
        params,
        p.assign_to_fields.unwrap_or(false),
        None,
    )?;
    let insert_at = java_after_fields_insert_position(class_node, &parsed.source);
    let plan = RefactorPlan {
        title: format!("Add Java constructor to {}", source_path.display()),
        kind: "add_java_constructor".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement: format!("\n\n{}", constructor.trim_end()),
            }],
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_move_java_field(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_java_field"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let source_parsed = parse_source_file(&source_path)?;
    let target_parsed = parse_source_file(&target_path)?;
    if source_parsed.language != "java" || target_parsed.language != "java" {
        bail!("move_java_field only supports java files");
    }
    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names (field names) is required for move_java_field"))?;
    let selected = select_java_fields_by_name(&source_parsed, names)?;
    let target_class = find_first_class_declaration(target_parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", target_path.display()))?;
    let insert_at = java_after_fields_insert_position(target_class, &target_parsed.source);
    let moved_text = selected
        .iter()
        .map(|field| {
            source_parsed.source[field.item.leading_trivia_start..field.item.byte_end]
                .trim_matches('\n')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut source_edits = selected
        .iter()
        .map(|field| TextEdit {
            byte_start: field.item.leading_trivia_start,
            byte_end: field.item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;
    let moved_decl_ranges = selected
        .iter()
        .map(|field| (field.item.byte_start, field.item.byte_end))
        .collect::<Vec<_>>();
    let moved_field_names = selected
        .iter()
        .map(|field| field.name.clone())
        .collect::<Vec<_>>();
    let remaining_source_accessors = if p.deep_analysis.unwrap_or(false) {
        compute_remaining_source_accessors(&source_parsed, &moved_field_names, &moved_decl_ranges)
    } else {
        Vec::new()
    };
    let plan = RefactorPlan {
        title: format!(
            "Move {} Java field(s) from {} to {}",
            selected.len(),
            source_path.display(),
            target_path.display()
        ),
        kind: "move_java_field".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(source_parsed.source.as_bytes()),
                edits: source_edits,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_parsed.source.as_bytes()),
                edits: vec![TextEdit {
                    byte_start: insert_at,
                    byte_end: insert_at,
                    replacement: format!("\n{}", moved_text),
                }],
            },
        ],
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: selected.into_iter().map(|field| field.item).collect(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors,
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_move_java_constant(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_java_constant"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let source_parsed = parse_source_file(&source_path)?;
    if source_parsed.language != "java" {
        bail!("move_java_constant only supports java files");
    }
    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names (constant names) is required for move_java_constant"))?;
    let visibility = p.visibility.as_deref().unwrap_or("private").to_string();
    validate_java_visibility(&visibility)?;
    let keep_copy = p.keep_copy.unwrap_or(false);

    // Match each name against a static-final field_declaration.
    let selected = select_java_static_final_fields_by_name(&source_parsed, names)?;

    // Build moved constant text(s) with the requested visibility.
    let moved_text = selected
        .iter()
        .map(|info| render_java_static_final_with_visibility(info, &visibility))
        .collect::<Vec<_>>()
        .join("\n");

    // Source-side edits: either remove the declaration or rewrite its
    // visibility (when keep_copy is true and current visibility is tighter
    // than `package`).
    let mut source_edits = Vec::new();
    for info in &selected {
        if keep_copy {
            if let Some(edit) = widen_static_final_visibility_edit(info, &source_parsed.source) {
                source_edits.push(edit);
            }
        } else {
            // Use leading_trivia_start..(end-of-line-after-byte_end) so back-to-back
            // declarations produce adjacent (not overlapping) edits — trailing_trivia_end
            // greedily consumes the next line's indentation and would overlap the
            // following declaration's leading_trivia_start.
            let end = end_of_line_after(&source_parsed.source, info.field.item.byte_end);
            source_edits.push(TextEdit {
                byte_start: info.field.item.leading_trivia_start,
                byte_end: end,
                replacement: String::new(),
            });
        }
    }
    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Target file: create-if-missing, mirroring extract_java_methods.
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    let target_content = if !original_target_bytes.is_empty() {
        let target_parsed = parse_source_file(&target_path)?;
        if target_parsed.language != "java" {
            bail!("move_java_constant only supports java files");
        }
        let target_class = find_first_class_declaration(target_parsed.tree.root_node())
            .ok_or_else(|| {
                anyhow!("no class declaration found in {}", target_path.display())
            })?;
        let insert_at = java_after_fields_insert_position(target_class, &target_parsed.source);
        let mut text = target_parsed.source.clone();
        text.insert_str(insert_at, &format!("\n{}", moved_text));
        text
    } else {
        let class_name = java_target_type_name(p, &target_path)?;
        let prelude = java_default_target_prelude(p, &source_parsed.source);
        // Indent constant declarations to match class-body conventions.
        let body = moved_text
            .lines()
            .map(|line| {
                if line.is_empty() {
                    line.to_string()
                } else {
                    format!("    {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        java_class_wrapper(&class_name, &prelude, &body)
    };

    let mut edits = Vec::new();
    if !source_edits.is_empty() {
        edits.push(FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source_parsed.source.as_bytes()),
            edits: source_edits,
        });
    }
    edits.push(FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
    });

    let plan = RefactorPlan {
        title: format!(
            "Move {} Java constant(s) from {} to {}",
            selected.len(),
            source_path.display(),
            target_path.display()
        ),
        kind: "move_java_constant".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits,
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: selected.into_iter().map(|info| info.field.item).collect(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
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
                child.kind() == "variable_declarator"
                    || child.kind() == "variable_declarator_id"
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

/// If `node` is an identifier, return the access expression we should
/// consider for kind classification: either the identifier itself (bare
/// access) or the enclosing `field_access` when the identifier sits in the
/// `field` position of a `this.<name>` or `(...).<name>` expression where
/// the object is `this`. Returns None when the identifier is not actually a
/// field-resolved access (method names, type names, declarators, accesses
/// on other objects, etc.).
fn resolve_field_access<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let parent = node.parent()?;
    let parent_kind = parent.kind();

    // Reject identifiers that are *names* of declarations or invocations.
    match parent_kind {
        "variable_declarator" => {
            if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) {
                return None;
            }
        }
        "formal_parameter"
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
        | "enum_constant" => {
            if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) {
                return None;
            }
        }
        "method_invocation" => {
            // Reject when the identifier *is* the method name.
            if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) {
                return None;
            }
            // Otherwise (object position, etc.) it's a field/variable read.
        }
        "method_reference" => {
            // `Foo::bar` — `Foo` could be a class type (skip) or an instance
            // field; tree-sitter labels both as identifiers. Instance field
            // followed by `::method` is rare for a moved field; skip to avoid
            // false positives. The method-name part (after ::) is not an
            // identifier in the field-name sense either.
            return None;
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
        "scoped_identifier"
        | "scoped_type_identifier"
        | "type_identifier"
        | "generic_type" => {
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

pub(crate) fn plan_update_java_callers(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("update_java_callers only supports java files");
    }
    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required for update_java_callers"))?;
    validate_java_member_name(delegate_field, "delegate_field")?;
    let methods = p
        .item_names
        .as_deref()
        .filter(|methods| !methods.is_empty())
        .ok_or_else(|| anyhow!("item_names (method names) is required for update_java_callers"))?;
    let edits = java_caller_rewrite_edits(&parsed, methods, delegate_field, &[])?;
    if edits.is_empty() {
        bail!("no matching Java call sites found");
    }
    let plan = RefactorPlan {
        title: format!(
            "Rewrite {} Java call site(s) through {} in {}",
            edits.len(),
            delegate_field,
            source_path.display()
        ),
        kind: "update_java_callers".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
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
                                let prefix = parsed.source
                                    [node.start_byte()..name_node.start_byte()]
                                    .trim();
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

pub(crate) fn plan_add_java_delegate_field(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("add_java_delegate_field only supports java files");
    }
    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required for add_java_delegate_field"))?;
    let delegate_type = p
        .delegate_type
        .as_deref()
        .or(p.module_name.as_deref())
        .ok_or_else(|| {
            anyhow!("delegate_type or module_name is required for add_java_delegate_field")
        })?;
    validate_java_member_name(delegate_field, "delegate_field")?;
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
    let field_insert_at = java_after_fields_insert_position(class_node, &parsed.source);
    let field_decl = format!("    private final {delegate_type} {delegate_field};");
    let constructor_args = p
        .parameters
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|param| param.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let assignment = format!("this.{delegate_field} = new {delegate_type}({constructor_args});");
    let mut edits = vec![TextEdit {
        byte_start: field_insert_at,
        byte_end: field_insert_at,
        replacement: format!("\n{field_decl}"),
    }];
    if let Some(constructor) = first_constructor_node(class_node, &parsed.source) {
        let insert_at = constructor_body_insert_position(constructor, &parsed.source);
        edits.push(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement: format!("\n        {assignment}"),
        });
    } else {
        let class_name = java_class_name(class_node, &parsed.source);
        let constructor = java_constructor_decl(
            &class_name,
            p.visibility.as_deref().unwrap_or("public"),
            &[],
            false,
            Some(&assignment),
        )?;
        edits[0].replacement = format!("\n{field_decl}\n\n{}", constructor.trim_end());
    }
    edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&edits)?;
    let plan = RefactorPlan {
        title: format!(
            "Add Java delegate field {} to {}",
            delegate_field,
            source_path.display()
        ),
        kind: "add_java_delegate_field".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_rewrite_java_visibility(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("rewrite_java_visibility only supports java files");
    }

    let target_visibility = p
        .visibility
        .as_deref()
        .ok_or_else(|| anyhow!("visibility is required for rewrite_java_visibility (one of: public, protected, private, package"))?;
    if !matches!(
        target_visibility,
        "public" | "protected" | "private" | "package"
    ) {
        bail!("visibility must be one of: public, protected, private, package; got `{target_visibility}`");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    if names.is_empty() {
        bail!("item_names (method or field names) must be provided for rewrite_java_visibility");
    }

    let candidates = java_methods(&parsed);
    let mut selected_nodes: Vec<Node<'_>> = Vec::new();

    for expected in names {
        let matches: Vec<&JavaMethod> = candidates
            .iter()
            .filter(|m| m.item.name.as_deref() == Some(expected))
            .collect();
        match matches.as_slice() {
            [] => bail!("requested method `{expected}` was not found"),
            [method] => {
                let node = parsed.tree.root_node();
                let method_node = find_node(node, |n: Node<'_>| {
                    n.kind() == "method_declaration"
                        && n.start_byte() == method.item.byte_start
                        && n.end_byte() == method.item.byte_end
                });
                if let Some(mn) = method_node {
                    selected_nodes.push(mn);
                } else {
                    bail!("could not locate AST node for method `{expected}`");
                }
            }
            _ => bail!(
                "requested method `{expected}` matched multiple methods; overloading requires more specific targeting"
            ),
        }
    }

    let mut edits = Vec::new();
    for method_node in &selected_nodes {
        let current_mods = collect_java_modifiers(*method_node);
        let current_vis = java_visibility_from_mods(&current_mods);

        if current_vis == target_visibility {
            continue;
        }

        if target_visibility == "package" {
            edits.push(build_visibility_rewrite_edit(
                *method_node,
                &current_mods,
                None,
                &parsed.source,
            ));
        } else {
            edits.push(build_visibility_rewrite_edit(
                *method_node,
                &current_mods,
                Some(target_visibility),
                &parsed.source,
            ));
        }
    }

    if edits.is_empty() {
        bail!("all selected methods already have the requested visibility");
    }

    edits.sort_by_key(|e| e.byte_start);

    let plan = RefactorPlan {
        title: format!(
            "Rewrite visibility of {} method(s) to {} in {}",
            edits.len(),
            target_visibility,
            source_path.display()
        ),
        kind: "rewrite_java_visibility".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
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
            if new_visibility.is_some() {
                TextEdit {
                    byte_start: vis.1,
                    byte_end: vis.2,
                    replacement: new_visibility.unwrap().to_string(),
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

pub(crate) fn plan_add_java_implements(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("add_java_implements only supports java files");
    }

    let interface_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name is required for add_java_implements (fully-qualified or simple interface name)"))?;

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
        .unwrap_or("(unnamed)")
        .to_string();

    let implements_pos = find_implements_position(class_node);
    let open_brace_pos = find_open_brace_position(class_node, &parsed.source);

    let (insert_pos, insert_text) = if let Some(pos) = implements_pos {
        let existing_impl = &parsed.source[pos..open_brace_pos];
        let trimmed = existing_impl.trim();
        if trimmed.contains('{') {
            let brace_relative = existing_impl.find('{').unwrap_or(0);
            (pos + brace_relative, format!(", {}", interface_name))
        } else {
            (pos, format!(", {}", interface_name))
        }
    } else {
        let name_node = class_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("class has no name node"))?;
        let after_name = name_node.end_byte();
        (after_name, format!(" implements {}", interface_name))
    };

    let edit = TextEdit {
        byte_start: insert_pos,
        byte_end: insert_pos,
        replacement: insert_text,
    };

    let plan = RefactorPlan {
        title: format!("Add implements {} to class {}", interface_name, class_name),
        kind: "add_java_implements".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_java_interface(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!(
                "target is required for extract_java_interface (path for the new interface file)"
            )
        })
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_interface only supports java files");
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
        .unwrap_or("(unnamed)")
        .to_string();

    let interface_name = p
        .module_name
        .as_deref()
        .unwrap_or_else(|| {
            if class_name.starts_with("Default") && class_name.len() > 7 {
                &class_name[7..]
            } else {
                &class_name
            }
        })
        .to_string();

    let package_decl = extract_java_package(&parsed.source)
        .map(|pkg| format!("package {};\n", pkg))
        .unwrap_or_default();

    let methods = java_methods(&parsed);
    let class_methods: Vec<&JavaMethod> = methods
        .iter()
        .filter(|m| m.parent_name == class_name)
        .collect();

    let selected_names: HashSet<&str> = if let Some(names) = p.item_names.as_deref() {
        names.iter().map(String::as_str).collect()
    } else {
        class_methods
            .iter()
            .filter(|m| {
                let method_node = parsed.tree.root_node();
                let node = find_node(method_node, |n: Node<'_>| {
                    n.kind() == "method_declaration"
                        && n.start_byte() == m.item.byte_start
                        && n.end_byte() == m.item.byte_end
                });
                node.is_some()
                    && node.unwrap().kind() == "method_declaration"
                    && method_is_public(node.unwrap())
                    && !method_is_static(node.unwrap())
                    && m.item.name.as_deref() != Some(&format!("<init>"))
            })
            .filter_map(|m| m.item.name.as_deref())
            .collect()
    };

    if selected_names.is_empty() {
        bail!("no public non-static methods found to extract; pass item_names to select specific methods");
    }

    let mut interface_sigs = Vec::new();
    let mut methods_needing_visibility_widen = Vec::new();
    let mut class_type_params: HashSet<String> = HashSet::new();
    if let Some(tp_text) = class_type_parameters_text(class_node, &parsed.source) {
        for chunk in tp_text
            .trim_start_matches('<')
            .trim_end_matches('>')
            .split(',')
        {
            let ident = chunk.trim().split_whitespace().last().unwrap_or("").trim();
            if !ident.is_empty() {
                class_type_params.insert(ident.to_string());
            }
        }
    }

    let mut used_type_params: HashSet<String> = HashSet::new();

    for method in &class_methods {
        let name = match method.item.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        if !selected_names.contains(name) {
            continue;
        }

        let method_node = find_node(parsed.tree.root_node(), |n: Node<'_>| {
            n.kind() == "method_declaration"
                && n.start_byte() == method.item.byte_start
                && n.end_byte() == method.item.byte_end
        });
        let method_node = match method_node {
            Some(n) => n,
            None => continue,
        };

        if name == class_name {
            continue;
        }

        let is_pub = method_is_public(method_node);
        if !is_pub {
            methods_needing_visibility_widen.push(method.item.byte_start);
        }

        let sig = extract_interface_method_signature(method_node, &parsed.source);
        interface_sigs.push(sig);

        let sig_type_params = collect_type_params_in_signature(method_node, &parsed.source);
        for tp in &sig_type_params {
            if class_type_params.contains(tp.as_str()) {
                used_type_params.insert(tp.clone());
            }
        }
    }

    if interface_sigs.is_empty() {
        bail!("no valid method signatures could be extracted");
    }

    let interface_type_params = if !used_type_params.is_empty() {
        let mut ordered = Vec::new();
        if let Some(tp_text) = class_type_parameters_text(class_node, &parsed.source) {
            let inner = tp_text.trim_start_matches('<').trim_end_matches('>');
            for chunk in inner.split(',') {
                let ident = chunk.trim().split_whitespace().last().unwrap_or("").trim();
                if used_type_params.contains(ident) {
                    ordered.push(chunk.trim().to_string());
                }
            }
        }
        if ordered.is_empty() {
            String::new()
        } else {
            format!("<{}>", ordered.join(", "))
        }
    } else {
        String::new()
    };

    let mut interface_content = String::new();
    if !package_decl.is_empty() {
        interface_content.push_str(&package_decl);
        interface_content.push('\n');
    }
    interface_content.push_str(&format!(
        "public interface {}{} {{\n",
        interface_name, interface_type_params
    ));
    for sig in &interface_sigs {
        for line in sig.lines() {
            interface_content.push_str(&format!("    {}\n", line));
        }
        interface_content.push('\n');
    }
    if interface_content.ends_with("\n\n") {
        interface_content.pop();
    }
    interface_content.push_str("}\n");

    let mut edits = Vec::new();

    edits.push(FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&[]),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: interface_content,
        }],
    });

    let implements_pos = find_implements_position(class_node);
    let open_brace_pos = find_open_brace_position(class_node, &parsed.source);

    let (impl_insert_pos, impl_insert_text) = if let Some(pos) = implements_pos {
        let existing_impl = &parsed.source[pos..open_brace_pos];
        if existing_impl.contains('{') {
            let brace_relative = existing_impl.find('{').unwrap_or(0);
            (pos + brace_relative, format!(", {}", interface_name))
        } else {
            (pos, format!(", {}", interface_name))
        }
    } else {
        let name_node = class_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("class has no name node"))?;
        let after_name = name_node.end_byte();
        (after_name, format!(" implements {}", interface_name))
    };

    let mut source_edits = vec![TextEdit {
        byte_start: impl_insert_pos,
        byte_end: impl_insert_pos,
        replacement: impl_insert_text,
    }];

    for method_start in &methods_needing_visibility_widen {
        let method_node = find_node(parsed.tree.root_node(), |n: Node<'_>| {
            n.kind() == "method_declaration" && n.start_byte() == *method_start
        });
        if let Some(mn) = method_node {
            let mods = collect_java_modifiers(mn);
            let edit = build_visibility_rewrite_edit(mn, &mods, Some("public"), &parsed.source);
            source_edits.push(edit);
        }
    }

    source_edits.sort_by_key(|e| e.byte_start);

    edits.push(FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
    });

    let validations = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!(
            "Extract interface {} from class {} ({} methods), add implements clause",
            interface_name,
            class_name,
            interface_sigs.len()
        ),
        kind: "extract_java_interface".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_migrate_java_type_usages(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("migrate_java_type_usages only supports java files");
    }

    let class_name = p.module_name.as_deref().ok_or_else(|| {
        anyhow!(
            "module_name is required for migrate_java_type_usages (simple class name to replace)"
        )
    })?;

    let new_name = p.new_text.as_deref().ok_or_else(|| {
        anyhow!("new_text is required for migrate_java_type_usages (replacement type name)")
    })?;

    let _source_class = extract_java_package(&parsed.source)
        .map(|pkg| format!("{}.{}", pkg, class_name))
        .unwrap_or_else(|| class_name.to_string());

    let mut edits = Vec::new();
    let positions = find_type_use_positions_in_file(&parsed.source, &parsed.tree, class_name);

    for (start, end) in positions {
        let context_start = start.saturating_sub(40);
        let context_end = (end + 40).min(parsed.source.len());
        let after = &parsed.source[end..context_end];
        let trimmed_after = after.trim_start();
        if trimmed_after.starts_with('.') || trimmed_after.starts_with('(') {
            continue;
        }
        let before = &parsed.source[context_start..start];
        if before.ends_with("new ") || before.ends_with("new\t") {
            continue;
        }

        edits.push(TextEdit {
            byte_start: start,
            byte_end: end,
            replacement: new_name.to_string(),
        });
    }

    if edits.is_empty() {
        bail!(
            "no type-use positions found for `{}` in {}",
            class_name,
            source_path.display()
        );
    }

    edits.sort_by_key(|e| e.byte_start);
    let n = edits.len();

    let plan = RefactorPlan {
        title: format!(
            "Migrate {} type-use(s) of {} to {} in {}",
            n,
            class_name,
            new_name,
            source_path.display()
        ),
        kind: "migrate_java_type_usages".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

use lsp_types::{
    request::CodeActionRequest, CodeActionContext, CodeActionKind, CodeActionParams, Position,
    Range, TextDocumentIdentifier,
};

use crate::lsp::{LspError, LspSessionManager};
use crate::projects::Language;

/// Ask JDTLS for `source.organizeImports` code actions on
/// `source_path` using the shared session pool. The session is
/// lazily spawned on first call for `(project_dir, Java)` and reused
/// across subsequent calls.
pub(crate) fn jdtls_organize_imports(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<FileEdit>> {
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let end_position = byte_to_lsp_position(&source, source.len());
    let code_action_params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: end_position,
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let response = manager.with_session(project_dir, Language::Java, |mut client| {
        let id = client.send_request::<CodeActionRequest>(&code_action_params)?;
        client
            .read_response::<CodeActionRequest>(id)
            .map_err(|e| match e {
                LspError::Broken(b) => LspError::Broken(b),
                LspError::Other(o) => LspError::Other(o),
            })
    })?;

    let mut all_edits = Vec::new();
    if let Some(actions) = response {
        for action in actions {
            if let lsp_types::CodeActionOrCommand::CodeAction(ca) = action {
                let kind = ca
                    .kind
                    .clone()
                    .unwrap_or_else(|| lsp_types::CodeActionKind::from(""));
                if kind == CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                    || ca.title.to_ascii_lowercase().contains("organize")
                {
                    if let Some(edit) = ca.edit {
                        all_edits.extend(workspace_edit_to_file_edits(edit)?);
                    }
                }
            }
        }
    }
    Ok(all_edits)
}

pub(crate) fn plan_java_lsp_organize_imports(
    p: &RefactorPlanParams,
    ctx: &PlanContext,
) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let project_dir_str = project_dir.to_string_lossy();
    let source_path = resolve_path(Some(&project_dir_str), &p.source)?;

    let lsp_attempt = ctx
        .lsp
        .as_ref()
        .map(|mgr| jdtls_organize_imports(mgr, &project_dir, &source_path));
    let (file_edits, semantic_status) = match lsp_attempt {
        Some(Ok(edits)) if !edits.is_empty() => (edits, SemanticStatus::LspVerified),
        _ => {
            if let Some(Err(err)) = &lsp_attempt {
                tracing::debug!(error = %err, "JDTLS organize_imports failed; falling back to heuristic");
            }
            (
                heuristic_java_organize_imports(&project_dir, &source_path)?,
                SemanticStatus::StructuralOnly,
            )
        }
    };
    if file_edits.is_empty() {
        bail!("no Java import organization edits needed");
    }

    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!("Organize Java imports in {}", p.source),
        kind: "java_lsp_organize_imports".to_string(),
        semantic_status,
        dry_run: false,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            registered_at: "2026-05-09T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
        }
    }

    fn java_plan_params(kind: &str, source: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: kind.to_string(),
            source: path_string(source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: None,
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            project_dir: None,
        }
    }

    #[test]
    fn java_status_items_include_methods_and_nested_classes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Example.java");
        fs::write(
            &path,
            "class Example {\n    void run() {}\n    class Nested { int value() { return 1; } }\n}\n",
        )
        .unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.language, "java");
        assert!(parsed.items.iter().any(|item| {
            item.kind == "class_declaration" && item.name.as_deref() == Some("Example")
        }));
        assert!(parsed.items.iter().any(|item| {
            item.kind == "method_declaration" && item.name.as_deref() == Some("run")
        }));
        assert!(parsed.items.iter().any(|item| {
            item.kind == "class_declaration" && item.name.as_deref() == Some("Nested")
        }));
    }

    #[test]
    fn extract_java_methods_creates_missing_target_class() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("God.java");
        let target = dir.path().join("ExtractedMethods.java");
        fs::write(
            &source,
            "package com.example;\n\nimport java.util.List;\n\npublic class God {\n    List<String> run() { return List.of(); }\n    void keep() { }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_methods", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["run".to_string()]);

        let plan_text = plan_extract_java_methods(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("package com.example;"));
        assert!(target_text.contains("import java.util.List;"));
        assert!(target_text.contains("public class ExtractedMethods"));
        assert!(target_text.contains("List<String> run()"));
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("List<String> run()"));
        assert!(source_text.contains("void keep()"));
    }

    #[test]
    fn extract_java_methods_reports_captured_source_fields() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Dashboard.java");
        let target = dir.path().join("ExtractedGrid.java");
        fs::write(
            &source,
            "class Dashboard {\n    private final Admin admin;\n    private Grid grid;\n    void moveMe() { grid = admin.load(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_methods", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["moveMe".to_string()]);

        let plan_text = plan_extract_java_methods(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert!(plan.captured_variables.iter().any(|capture| {
            capture.name == "admin"
                && capture.source_type == "Admin"
                && capture.source_visibility == "private final"
        }));
        assert!(plan.captured_variables.iter().any(|capture| {
            capture.name == "grid"
                && capture.source_type == "Grid"
                && capture.source_visibility == "private"
        }));
    }

    // -----------------------------------------------------------------
    // Gaps 12, 14, 15: external_calls + inherited_dependencies reports.
    // -----------------------------------------------------------------

    fn extract_dependency_plan(
        project_dir: &Path,
        source: &Path,
        target: &Path,
        item_names: &[&str],
    ) -> RefactorPlan {
        let mut params = java_plan_params("extract_java_methods", source);
        params.target = Some(path_string(target));
        params.item_names = Some(item_names.iter().map(|n| n.to_string()).collect());
        params.project_dir = Some(path_string(project_dir));
        params.deep_analysis = Some(true);
        let plan_text = plan_extract_java_methods(&params).unwrap();
        serde_json::from_str(&plan_text).unwrap()
    }

    #[test]
    fn extract_java_methods_reports_external_call_to_source_class_method() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("CompositionView.java");
        let target = dir.path().join("MeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\
             class CompositionView {\n\
            \x20   List<Item> getHistoryItemsBySamplePoint(Point p) { return List.of(); }\n\
            \x20   void createSamplePointStatusBadge() {\n\
            \x20       List<Item> items = getHistoryItemsBySamplePoint(null);\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let plan =
            extract_dependency_plan(dir.path(), &source, &target, &["createSamplePointStatusBadge"]);
        let call = plan
            .external_calls
            .iter()
            .find(|c| c.method == "getHistoryItemsBySamplePoint")
            .expect("external call missing");
        assert!(
            call.signature.contains("List<Item>")
                && call.signature.contains("getHistoryItemsBySamplePoint")
                && call.signature.contains("(Point p)"),
            "signature was {}",
            call.signature
        );
        assert!(!call.signature_partial);
        assert_eq!(call.call_sites.len(), 1);
        assert_eq!(call.call_sites[0].in_method, "createSamplePointStatusBadge");
        assert_eq!(call.call_sites[0].context, "direct");
    }

    #[test]
    fn extract_java_methods_reports_inherited_interface_method() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("HasLogger.java"),
            "package p;\npublic interface HasLogger {\n    Logger getLogger();\n}\n",
        )
        .unwrap();
        let source = dir.path().join("CompositionView.java");
        fs::write(
            &source,
            "package p;\n\
             public class CompositionView implements HasLogger {\n\
            \x20   void createSamplePointStatusBadge() { getLogger().info(\"x\"); }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("MeterGrid.java");
        let plan =
            extract_dependency_plan(dir.path(), &source, &target, &["createSamplePointStatusBadge"]);
        let inherited = plan
            .inherited_dependencies
            .iter()
            .find(|d| d.method == "getLogger")
            .expect("inherited getLogger missing");
        assert_eq!(inherited.source, "HasLogger");
        assert_eq!(inherited.source_kind, "interface");
        assert_eq!(inherited.call_sites.len(), 1);
        assert_eq!(inherited.call_sites[0].context, "direct");
    }

    #[test]
    fn extract_java_methods_reports_inherited_superclass_method() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("BaseView.java"),
            "package p;\npublic class BaseView {\n    public void applyFilters() {}\n}\n",
        )
        .unwrap();
        let source = dir.path().join("CompositionView.java");
        fs::write(
            &source,
            "package p;\n\
             public class CompositionView extends BaseView {\n\
            \x20   void createMeterGrid() { applyFilters(); }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("MeterGrid.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["createMeterGrid"]);
        let inherited = plan
            .inherited_dependencies
            .iter()
            .find(|d| d.method == "applyFilters")
            .expect("inherited applyFilters missing");
        assert_eq!(inherited.source, "BaseView");
        assert_eq!(inherited.source_kind, "class");
    }

    #[test]
    fn extract_java_methods_resolves_multi_hop_inheritance_to_actual_declarer() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Base.java"),
            "package p;\npublic class Base {\n    public void rootHook() {}\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Mid.java"),
            "package p;\npublic class Mid extends Base {}\n",
        )
        .unwrap();
        let source = dir.path().join("Leaf.java");
        fs::write(
            &source,
            "package p;\n\
             public class Leaf extends Mid {\n\
            \x20   void doIt() { rootHook(); }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("Other.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["doIt"]);
        let inherited = plan
            .inherited_dependencies
            .iter()
            .find(|d| d.method == "rootHook")
            .expect("inherited rootHook missing");
        assert_eq!(inherited.source, "Base");
        assert_eq!(inherited.source_kind, "class");
    }

    #[test]
    fn extract_java_methods_marks_lambda_calls_with_lambda_context() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("View.java");
        fs::write(
            &source,
            "package p;\n\
             class View {\n\
            \x20   void refreshSamplePointItem() {}\n\
            \x20   void createTrackChangeDialog() {\n\
            \x20       Runnable r = () -> refreshSamplePointItem();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("Other.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["createTrackChangeDialog"]);
        let call = plan
            .external_calls
            .iter()
            .find(|c| c.method == "refreshSamplePointItem")
            .expect("expected refreshSamplePointItem in external_calls");
        assert_eq!(call.call_sites.len(), 1);
        assert_eq!(call.call_sites[0].context, "lambda");
    }

    #[test]
    fn extract_java_methods_marks_direct_calls_with_direct_context() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("View.java");
        fs::write(
            &source,
            "package p;\n\
             class View {\n\
            \x20   void refresh() {}\n\
            \x20   void run() { refresh(); }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("Other.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["run"]);
        let call = plan
            .external_calls
            .iter()
            .find(|c| c.method == "refresh")
            .expect("refresh missing");
        assert_eq!(call.call_sites[0].context, "direct");
    }

    #[test]
    fn extract_java_methods_omits_jdk_calls() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("View.java");
        fs::write(
            &source,
            "package p;\n\
             class View {\n\
            \x20   void run() { System.out.println(\"hi\"); String.valueOf(1); }\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("Other.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["run"]);
        assert!(
            plan.external_calls.is_empty(),
            "expected empty external_calls, got {:?}",
            plan.external_calls
        );
        assert!(plan.inherited_dependencies.is_empty());
    }

    #[test]
    fn extract_java_methods_omits_calls_to_other_extracted_methods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("View.java");
        fs::write(
            &source,
            "package p;\n\
             class View {\n\
            \x20   void run() { helper(); }\n\
            \x20   void helper() {}\n\
             }\n",
        )
        .unwrap();
        let target = dir.path().join("Other.java");
        let plan = extract_dependency_plan(dir.path(), &source, &target, &["run", "helper"]);
        assert!(
            plan.external_calls
                .iter()
                .all(|c| c.method != "helper"),
            "helper should be internal, not external: {:?}",
            plan.external_calls
        );
    }

    #[test]
    fn java_gap_primitives_plan_guarded_edits() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Source.java");
        let target = dir.path().join("Target.java");
        fs::write(
            &source,
            "class Source {\n    private Grid grid;\n    Source(Dep dep) { setup(); this.refresh(); }\n    void setup() { refresh(); }\n    void refresh() {}\n}\n",
        )
        .unwrap();
        fs::write(&target, "class Target {\n}\n").unwrap();

        let mut add_fields = java_plan_params("add_java_fields", &target);
        add_fields.fields = Some(vec![JavaFieldSpec {
            visibility: Some("private".to_string()),
            type_name: "Dep".to_string(),
            name: "dep".to_string(),
            final_field: Some(true),
        }]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_add_java_fields(&add_fields).unwrap()).unwrap();
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("private final Dep dep;"));

        let mut constructor = java_plan_params("add_java_constructor", &target);
        constructor.parameters = Some(vec![JavaParameterSpec {
            type_name: "Dep".to_string(),
            name: "dep".to_string(),
        }]);
        constructor.assign_to_fields = Some(true);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_add_java_constructor(&constructor).unwrap()).unwrap();
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("this.dep = dep;"));

        let mut callers = java_plan_params("update_java_callers", &source);
        callers.delegate_field = Some("target".to_string());
        callers.item_names = Some(vec!["refresh".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();
        assert_eq!(plan.edits[0].edits.len(), 2);
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "target."));

        let mut move_field = java_plan_params("move_java_field", &source);
        move_field.target = Some(path_string(&target));
        move_field.item_names = Some(vec!["grid".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_move_java_field(&move_field).unwrap()).unwrap();
        assert_eq!(plan.edits.len(), 2);
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;"));

        let mut delegate = java_plan_params("add_java_delegate_field", &source);
        delegate.delegate_field = Some("target".to_string());
        delegate.delegate_type = Some("Target".to_string());
        delegate.parameters = Some(vec![JavaParameterSpec {
            type_name: "Dep".to_string(),
            name: "dep".to_string(),
        }]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_add_java_delegate_field(&delegate).unwrap()).unwrap();
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("private final Target target;")));
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("this.target = new Target(dep);")));
    }

    #[test]
    fn extract_java_class_composite_builds_source_and_target_edits() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Dashboard.java");
        let target = dir.path().join("ExtractedGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Dashboard {\n    private final Admin admin;\n    private Grid grid;\n    Dashboard() { grid = buildGrid(); refreshGrid(); }\n    Grid buildGrid() { return admin.load(); }\n    void refreshGrid() { grid.refresh(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("ExtractedGrid".to_string());
        params.delegate_field = Some("extractedGrid".to_string());
        params.item_names = Some(vec!["buildGrid".to_string(), "refreshGrid".to_string()]);
        params.move_fields = Some(vec!["grid".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan.kind, "extract_java_class");
        assert_eq!(plan.edits.len(), 2);
        assert!(plan
            .captured_variables
            .iter()
            .any(|capture| { capture.name == "admin" && capture.source_type == "Admin" }));
        assert!(plan.edits[0].edits.iter().any(|edit| edit
            .replacement
            .contains("private final ExtractedGrid extractedGrid;")));
        assert!(plan.edits[0].edits.iter().any(|edit| edit
            .replacement
            .contains("this.extractedGrid = new ExtractedGrid(admin);")));
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "extractedGrid."));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("public class ExtractedGrid"));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private final Admin admin;"));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;"));
    }

    #[test]
    fn update_java_callers_rewrites_method_references() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Source.java");
        fs::write(
            &source,
            "import java.util.List;\nimport java.util.stream.Stream;\n\
             class Source {\n\
            \x20   void wire(List<Integer> ints) {\n\
            \x20       ints.forEach(this::extractedMethod);\n\
            \x20       ints.stream().map(Foo::bar).count();\n\
            \x20       ints.forEach(super::extractedMethod);\n\
            \x20       extractedMethod(0);\n\
            \x20       this.extractedMethod(1);\n\
            \x20   }\n\
            \x20   void extractedMethod(int i) {}\n\
             }\n",
        )
        .unwrap();

        let mut callers = java_plan_params("update_java_callers", &source);
        callers.delegate_field = Some("delegate".to_string());
        callers.item_names = Some(vec!["extractedMethod".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();

        // Apply the edits in reverse order to get the rewritten text.
        let original = fs::read_to_string(&source).unwrap();
        let mut bytes = original.clone().into_bytes();
        let mut sorted = plan.edits[0].edits.clone();
        sorted.sort_by_key(|e| e.byte_start);
        for edit in sorted.iter().rev() {
            bytes.splice(
                edit.byte_start..edit.byte_end,
                edit.replacement.bytes(),
            );
        }
        let rewritten = String::from_utf8(bytes).unwrap();

        // Method-invocation rewrites still happen.
        assert!(
            rewritten.contains("delegate.extractedMethod(0)"),
            "unqualified call should be rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains("delegate.extractedMethod(1)"),
            "this-qualified call should be rewritten: {rewritten}"
        );

        // Method-reference: this::extractedMethod -> delegate::extractedMethod.
        assert!(
            rewritten.contains("delegate::extractedMethod"),
            "this-qualified method reference should be rewritten: {rewritten}"
        );
        assert!(
            !rewritten.contains("this::extractedMethod"),
            "this::extractedMethod should be gone: {rewritten}"
        );

        // Foo::bar must remain untouched (different receiver type).
        assert!(
            rewritten.contains("Foo::bar"),
            "static/external method reference must not be rewritten: {rewritten}"
        );

        // super::extractedMethod must remain untouched (super has different binding).
        assert!(
            rewritten.contains("super::extractedMethod"),
            "super:: reference must not be rewritten: {rewritten}"
        );
    }

    #[test]
    fn update_java_callers_method_reference_in_lambda_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Pipeline.java");
        fs::write(
            &source,
            "import java.util.List;\n\
             class Pipeline {\n\
            \x20   void run(List<String> xs) {\n\
            \x20       xs.stream().map(this::extractedMethod).forEach(System.out::println);\n\
            \x20   }\n\
            \x20   String extractedMethod(String s) { return s; }\n\
             }\n",
        )
        .unwrap();

        let mut callers = java_plan_params("update_java_callers", &source);
        callers.delegate_field = Some("delegate".to_string());
        callers.item_names = Some(vec!["extractedMethod".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();

        let original = fs::read_to_string(&source).unwrap();
        let mut bytes = original.into_bytes();
        let mut sorted = plan.edits[0].edits.clone();
        sorted.sort_by_key(|e| e.byte_start);
        for edit in sorted.iter().rev() {
            bytes.splice(
                edit.byte_start..edit.byte_end,
                edit.replacement.bytes(),
            );
        }
        let rewritten = String::from_utf8(bytes).unwrap();

        assert!(
            rewritten.contains("delegate::extractedMethod"),
            "this-qualified method reference inside lambda pipeline should be rewritten: {rewritten}"
        );
        // Unrelated method reference must stay.
        assert!(
            rewritten.contains("System.out::println"),
            "unrelated method reference must be preserved: {rewritten}"
        );
    }

    #[test]
    fn java_organize_imports_fallback_adds_project_type_import() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = dir.path().join("src/main/java/com/example/model");
        let ui_dir = dir.path().join("src/main/java/com/example/ui");
        fs::create_dir_all(&model_dir).unwrap();
        fs::create_dir_all(&ui_dir).unwrap();
        fs::write(
            model_dir.join("FooThing.java"),
            "package com.example.model;\n\npublic class FooThing {}\n",
        )
        .unwrap();
        let source = ui_dir.join("UsesFoo.java");
        fs::write(
            &source,
            "package com.example.ui;\n\npublic class UsesFoo {\n    private FooThing value;\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("java_lsp_organize_imports", &source);
        params.project_dir = Some(path_string(dir.path()));

        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan.kind, "java_lsp_organize_imports");
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("import com.example.model.FooThing;"));
    }

    #[test]
    fn move_java_constant_moves_three_constants_to_new_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    private static final String SAMPLE_STATUS_NOT_OK = \"OUT OF DATE\";\n    private static final String SAMPLE_STATUS_NO_DATASOURCE = \"NONE ASSIGNED\";\n    void keep() {}\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec![
            "SAMPLE_STATUS_OK".to_string(),
            "SAMPLE_STATUS_NOT_OK".to_string(),
            "SAMPLE_STATUS_NO_DATASOURCE".to_string(),
        ]);
        params.visibility = Some("private".to_string());
        params.module_name = Some("CompositionMeterGrid".to_string());

        let plan_text = plan_move_java_constant(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("SAMPLE_STATUS_OK"));
        assert!(!source_text.contains("SAMPLE_STATUS_NOT_OK"));
        assert!(!source_text.contains("SAMPLE_STATUS_NO_DATASOURCE"));
        assert!(source_text.contains("void keep()"));

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("public class CompositionMeterGrid"));
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_NOT_OK = \"OUT OF DATE\";"));
        assert!(target_text.contains(
            "private static final String SAMPLE_STATUS_NO_DATASOURCE = \"NONE ASSIGNED\";"
        ));
    }

    #[test]
    fn move_java_constant_keep_copy_widens_source_visibility_and_copies_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    void keep() {}\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
        params.visibility = Some("private".to_string());
        params.module_name = Some("CompositionMeterGrid".to_string());
        params.keep_copy = Some(true);

        let plan_text = plan_move_java_constant(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let source_text = fs::read_to_string(&source).unwrap();
        // Constant remains in source, but visibility was widened from
        // private to package (i.e. no visibility keyword).
        assert!(source_text.contains("static final String SAMPLE_STATUS_OK"));
        assert!(!source_text.contains("private static final String SAMPLE_STATUS_OK"));

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
    }

    #[test]
    fn move_java_constant_does_not_widen_when_keep_copy_false() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    private static final String OTHER = \"X\";\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
        params.visibility = Some("private".to_string());
        params.module_name = Some("CompositionMeterGrid".to_string());
        // keep_copy default false.

        let plan: RefactorPlan =
            serde_json::from_str(&plan_move_java_constant(&params).unwrap()).unwrap();
        // Source-side edits should remove the declaration (one removal edit),
        // not rewrite visibility on the surviving sibling.
        let source_edits = &plan.edits[0].edits;
        assert!(source_edits.iter().all(|edit| edit.replacement.is_empty()));
        // OTHER must remain untouched: no edit byte range covers it.
        let original = fs::read_to_string(&source).unwrap();
        let other_pos = original.find("OTHER").unwrap();
        assert!(source_edits.iter().all(|edit| {
            !(edit.byte_start <= other_pos && other_pos < edit.byte_end)
        }));
    }

    #[test]
    fn move_java_constant_rejects_non_static_final_field() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private String NOT_A_CONSTANT = \"x\";\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["NOT_A_CONSTANT".to_string()]);
        params.visibility = Some("private".to_string());
        params.module_name = Some("CompositionMeterGrid".to_string());

        let err = plan_move_java_constant(&params).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not declared as `static final`"), "got: {msg}");
        // Source unchanged on disk (plan returned an error before any apply).
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(source_text.contains("NOT_A_CONSTANT"));
    }

    #[test]
    fn move_java_constant_rejects_missing_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["DOES_NOT_EXIST".to_string()]);
        params.visibility = Some("private".to_string());
        params.module_name = Some("CompositionMeterGrid".to_string());

        let err = plan_move_java_constant(&params).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("DOES_NOT_EXIST"), "got: {msg}");
    }

    #[test]
    fn move_java_constant_appends_to_existing_target_class() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "package com.example;\n\npublic class CompositionMeterGrid {\n    private final Foo foo = new Foo();\n    void existing() {}\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("move_java_constant", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
        params.visibility = Some("public".to_string());

        let plan_text = plan_move_java_constant(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        // Existing declarations preserved.
        assert!(target_text.contains("private final Foo foo = new Foo();"));
        assert!(target_text.contains("void existing()"));
        // Constant inserted with the requested visibility.
        assert!(target_text
            .contains("public static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
    }

    fn move_field_plan_for(
        source_text: &str,
        target_text: &str,
        field_names: &[&str],
    ) -> RefactorPlan {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Source.java");
        let target = dir.path().join("Target.java");
        fs::write(&source, source_text).unwrap();
        fs::write(&target, target_text).unwrap();
        let mut params = java_plan_params("move_java_field", &source);
        params.target = Some(path_string(&target));
        params.item_names = Some(field_names.iter().map(|s| s.to_string()).collect());
        params.deep_analysis = Some(true);
        let plan_text = plan_move_java_field(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        // Keep tempdir alive for the duration of the test by leaking it; tests
        // are short-lived and cleanup happens on process exit.
        std::mem::forget(dir);
        plan
    }

    #[test]
    fn move_java_field_reports_remaining_reads_only() {
        let source = "class Source {\n    private Grid grid;\n    void show() {\n        view.add(grid);\n        render(grid);\n    }\n    void render(Grid g) {}\n}\n";
        let target = "class Target {\n}\n";
        let plan = move_field_plan_for(source, target, &["grid"]);
        let report = plan
            .remaining_source_accessors
            .iter()
            .find(|r| r.field == "grid")
            .expect("grid entry");
        assert_eq!(report.accesses.len(), 2);
        assert!(report.accesses.iter().all(|a| a.kind == "read"));
        assert_eq!(report.accesses[0].line, 4);
        assert!(report.accesses[0].context.contains("view.add(grid)"));
        assert_eq!(report.accesses[1].line, 5);
        assert!(report.accesses[1].context.contains("render(grid)"));
    }

    #[test]
    fn move_java_field_distinguishes_reads_and_writes() {
        let source = "class Source {\n    private int counter;\n    void bump() {\n        counter = counter + 1;\n        counter += 5;\n        counter++;\n        log(counter);\n    }\n    void log(int v) {}\n}\n";
        let target = "class Target {\n}\n";
        let plan = move_field_plan_for(source, target, &["counter"]);
        let report = plan
            .remaining_source_accessors
            .iter()
            .find(|r| r.field == "counter")
            .expect("counter entry");
        let writes = report
            .accesses
            .iter()
            .filter(|a| a.kind == "write")
            .count();
        let reads = report.accesses.iter().filter(|a| a.kind == "read").count();
        // 3 writes: `counter =`, `counter +=`, `counter++`.
        // 3 reads: rhs of `counter + 1`, log(counter), and (debatable) the
        // read embedded in `+=`. We only require classification of the LHS
        // positions reported as `write`, not the synthetic read of compound
        // assignment.
        assert!(writes >= 3, "expected >= 3 writes, got {writes} ({reads} reads)");
        assert!(reads >= 2, "expected >= 2 reads, got {reads} ({writes} writes)");
    }

    #[test]
    fn move_java_field_skips_local_shadowing() {
        let source = "class Source {\n    private int value;\n    void shadowed() {\n        int value = 7;\n        use(value);\n    }\n    void unshadowed() {\n        use(value);\n    }\n    void use(int v) {}\n}\n";
        let target = "class Target {\n}\n";
        let plan = move_field_plan_for(source, target, &["value"]);
        let report = plan
            .remaining_source_accessors
            .iter()
            .find(|r| r.field == "value")
            .expect("value entry");
        // Only the unshadowed read should be reported.
        assert_eq!(report.accesses.len(), 1, "report: {:?}", report.accesses);
        assert_eq!(report.accesses[0].line, 8);
    }

    #[test]
    fn move_java_field_reports_both_this_and_bare_access() {
        let source = "class Source {\n    private Grid grid;\n    void run() {\n        this.grid.refresh();\n        grid.show();\n    }\n}\n";
        let target = "class Target {\n}\n";
        let plan = move_field_plan_for(source, target, &["grid"]);
        let report = plan
            .remaining_source_accessors
            .iter()
            .find(|r| r.field == "grid")
            .expect("grid entry");
        assert_eq!(report.accesses.len(), 2);
        assert_eq!(report.accesses[0].line, 4);
        assert!(report.accesses[0].context.contains("this.grid.refresh()"));
        assert_eq!(report.accesses[1].line, 5);
        assert!(report.accesses[1].context.contains("grid.show()"));
    }

    #[test]
    fn move_java_field_with_no_remaining_accesses_reports_empty_list() {
        let source = "class Source {\n    private Grid grid;\n    void run() {}\n}\n";
        let target = "class Target {\n}\n";
        let plan = move_field_plan_for(source, target, &["grid"]);
        assert_eq!(plan.remaining_source_accessors.len(), 1);
        let report = &plan.remaining_source_accessors[0];
        assert_eq!(report.field, "grid");
        assert!(report.accesses.is_empty());
    }

    #[test]
    fn java_organize_imports_skips_inner_class_simple_name_import() {
        // Gap 16: `Outer.Inner` references must keep the qualified
        // form. The fallback must not synthesize `import x.Inner;`
        // when `Inner` only exists as a member of `Outer`'s body.
        let dir = tempfile::tempdir().unwrap();
        let view_dir = dir.path().join("src/main/java/com/example/view");
        let model_dir = dir.path().join("src/main/java/com/example/model");
        fs::create_dir_all(&view_dir).unwrap();
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(
            view_dir.join("CompositionView.java"),
            "package com.example.view;\n\npublic class CompositionView {\n    public static class SamplePointItemView {}\n}\n",
        )
        .unwrap();
        let source = model_dir.join("Helper.java");
        fs::write(
            &source,
            "package com.example.model;\n\nimport com.example.view.CompositionView;\n\npublic class Helper {\n    void use(CompositionView.SamplePointItemView item) {}\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("java_lsp_organize_imports", &source);
        params.project_dir = Some(path_string(dir.path()));

        let plan_result = plan_java_lsp_organize_imports(&params, &PlanContext::default());
        match plan_result {
            Ok(plan_text) => {
                let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
                let replacement = &plan.edits[0].edits[0].replacement;
                // No bare import for the inner class.
                assert!(
                    !replacement.contains("import com.example.model.SamplePointItemView;")
                        && !replacement.contains("import com.example.view.SamplePointItemView;"),
                    "fallback fabricated an inner-class import: {replacement}"
                );
                // The legitimate outer import is preserved.
                assert!(replacement.contains("import com.example.view.CompositionView;"));
            }
            Err(err) => {
                // The fallback may decide there are no edits to make
                // (which is the correct behavior here — the existing
                // outer import already covers the qualified ref).
                assert!(err.to_string().contains("no Java import organization edits needed"));
            }
        }
    }

    #[test]
    fn build_java_type_index_records_inner_classes() {
        let dir = tempfile::tempdir().unwrap();
        let view_dir = dir.path().join("src/com/x/view");
        fs::create_dir_all(&view_dir).unwrap();
        fs::write(
            view_dir.join("Outer.java"),
            "package com.x.view;\npublic class Outer { public static class Inner {} public interface IFoo {} }\n",
        )
        .unwrap();
        let idx = build_java_type_index(dir.path()).unwrap();
        assert!(idx.inner_class_names.contains("Inner"));
        assert!(idx.inner_class_names.contains("IFoo"));
        assert!(idx.top_level.get("Outer").is_some());
        // Top-level set must NOT include the inner names.
        assert!(idx.top_level.get("Inner").is_none());
    }

    // -----------------------------------------------------------------
    // Gap 20: extract_java_class moves static-final captures as constants
    // (preserving `static final` and the initializer) rather than promoting
    // them to instance fields + constructor parameters.
    // -----------------------------------------------------------------

    #[test]
    fn extract_java_class_moves_static_final_capture_as_constant() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Composition.java");
        let target = dir.path().join("CompositionMeterGrid.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    void render() { String s = SAMPLE_STATUS_OK; }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("CompositionMeterGrid".to_string());
        params.delegate_field = Some("compositionMeterGrid".to_string());
        params.item_names = Some(vec!["render".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

        // Captured variable carries source_static_final = true.
        let captured = plan
            .captured_variables
            .iter()
            .find(|c| c.name == "SAMPLE_STATUS_OK")
            .expect("SAMPLE_STATUS_OK should be in captured_variables");
        assert!(captured.source_static_final);

        // Target body contains the constant declaration with `static final`
        // and the original initializer literal.
        let target_replacement = &plan.edits[1].edits[0].replacement;
        assert!(
            target_replacement
                .contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"),
            "target should keep static final + initializer: {target_replacement}"
        );

        // No constructor parameter for the constant. The body should not
        // contain a `private final String SAMPLE_STATUS_OK;` instance field
        // line, and there should be no constructor at all (no other
        // captures).
        assert!(
            !target_replacement.contains("private final String SAMPLE_STATUS_OK;"),
            "target must not promote constant to instance field: {target_replacement}"
        );
        assert!(
            !target_replacement.contains("public CompositionMeterGrid("),
            "target must not synthesize a constructor for static-final captures: {target_replacement}"
        );

        // Source side: SAMPLE_STATUS_OK declaration is removed, and the
        // delegate constructor call does NOT pass SAMPLE_STATUS_OK.
        let original = fs::read_to_string(&source).unwrap();
        let mut bytes = original.into_bytes();
        let mut sorted = plan.edits[0].edits.clone();
        sorted.sort_by_key(|e| e.byte_start);
        for edit in sorted.iter().rev() {
            bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
        }
        let rewritten = String::from_utf8(bytes).unwrap();
        assert!(
            !rewritten.contains("private static final String SAMPLE_STATUS_OK"),
            "source should no longer declare the constant: {rewritten}"
        );
        assert!(
            !rewritten.contains("new CompositionMeterGrid(SAMPLE_STATUS_OK"),
            "source delegate call must not pass the constant: {rewritten}"
        );
    }

    #[test]
    fn extract_java_class_separates_static_final_from_instance_captures() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Mixed.java");
        let target = dir.path().join("MixedExtract.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Mixed {\n    private static final String LABEL = \"ok\";\n    private final Helper helper;\n    Mixed(Helper helper) { this.helper = helper; }\n    void render() { helper.use(LABEL); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("MixedExtract".to_string());
        params.delegate_field = Some("mixedExtract".to_string());
        params.item_names = Some(vec!["render".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        let target_replacement = &plan.edits[1].edits[0].replacement;

        // Constant: emitted as static final with initializer.
        assert!(
            target_replacement
                .contains("private static final String LABEL = \"ok\";"),
            "target should keep LABEL as static final constant: {target_replacement}"
        );
        assert!(
            !target_replacement.contains("private final String LABEL;"),
            "target must not promote LABEL to instance field: {target_replacement}"
        );

        // Instance capture `helper` becomes a constructor parameter and
        // assigned-to instance field on the target.
        assert!(
            target_replacement.contains("private final Helper helper;"),
            "target should hold helper as instance field: {target_replacement}"
        );
        assert!(
            target_replacement.contains("public MixedExtract(Helper helper)"),
            "target constructor should take Helper helper: {target_replacement}"
        );
        assert!(
            !target_replacement.contains("MixedExtract(String LABEL"),
            "target constructor must not include LABEL: {target_replacement}"
        );

        // Source-side constructor call passes only `helper`, not LABEL.
        let original = fs::read_to_string(&source).unwrap();
        let mut bytes = original.into_bytes();
        let mut sorted = plan.edits[0].edits.clone();
        sorted.sort_by_key(|e| e.byte_start);
        for edit in sorted.iter().rev() {
            bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
        }
        let rewritten = String::from_utf8(bytes).unwrap();
        assert!(
            rewritten.contains("new MixedExtract(helper)"),
            "source delegate call should pass only helper: {rewritten}"
        );
        assert!(
            !rewritten.contains("LABEL"),
            "source should no longer reference LABEL: {rewritten}"
        );
    }

    // -----------------------------------------------------------------
    // Gap 24: extract_java_class widens extracted-method visibility on the
    // target to at least `package` (or `public` when target is in a
    // different package than the source).
    // -----------------------------------------------------------------

    #[test]
    fn extract_java_class_widens_private_method_to_package_default() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Same.java");
        let target = dir.path().join("SameExtract.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Same {\n    private Grid createMeterGrid() { return new Grid(); }\n    void wire() { Grid g = createMeterGrid(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("SameExtract".to_string());
        params.delegate_field = Some("sameExtract".to_string());
        params.item_names = Some(vec!["createMeterGrid".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        let target_replacement = &plan.edits[1].edits[0].replacement;

        // The extracted method's `private` modifier is dropped (default
        // package visibility) so the source-side delegate call compiles.
        assert!(
            target_replacement.contains("Grid createMeterGrid()"),
            "method should still be present on target: {target_replacement}"
        );
        assert!(
            !target_replacement.contains("private Grid createMeterGrid()"),
            "private modifier should be widened to package: {target_replacement}"
        );
    }

    #[test]
    fn extract_java_class_widens_private_method_to_public_cross_package() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("a");
        let target_dir = dir.path().join("b");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        let source = source_dir.join("Cross.java");
        let target = target_dir.join("CrossExtract.java");
        fs::write(
            &source,
            "package com.a;\n\nclass Cross {\n    private Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("CrossExtract".to_string());
        params.delegate_field = Some("crossExtract".to_string());
        params.item_names = Some(vec!["createGrid".to_string()]);
        params.target_prelude = Some("package com.b;\n".to_string());

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        let target_replacement = &plan.edits[1].edits[0].replacement;

        assert!(
            target_replacement.contains("public Grid createGrid()"),
            "cross-package extraction should widen private to public: {target_replacement}"
        );
        assert!(
            !target_replacement.contains("private Grid createGrid()"),
            "private modifier must not survive cross-package extraction: {target_replacement}"
        );
    }

    #[test]
    fn extract_java_class_leaves_already_public_method_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Pub.java");
        let target = dir.path().join("PubExtract.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Pub {\n    public Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("PubExtract".to_string());
        params.delegate_field = Some("pubExtract".to_string());
        params.item_names = Some(vec!["createGrid".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        let target_replacement = &plan.edits[1].edits[0].replacement;

        assert!(
            target_replacement.contains("public Grid createGrid()"),
            "public method should be preserved verbatim: {target_replacement}"
        );
    }

    #[test]
    fn extract_java_class_keeps_protected_in_same_package() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Prot.java");
        let target = dir.path().join("ProtExtract.java");
        fs::write(
            &source,
            "package com.example;\n\nclass Prot {\n    protected Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

        let mut params = java_plan_params("extract_java_class", &source);
        params.target = Some(path_string(&target));
        params.module_name = Some("ProtExtract".to_string());
        params.delegate_field = Some("protExtract".to_string());
        params.item_names = Some(vec!["createGrid".to_string()]);

        let plan_text = plan_extract_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        let target_replacement = &plan.edits[1].edits[0].replacement;

        // protected (rank 2) is already above the package floor (1) — must
        // not be narrowed.
        assert!(
            target_replacement.contains("protected Grid createGrid()"),
            "protected should be preserved in same-package extraction: {target_replacement}"
        );
    }
}

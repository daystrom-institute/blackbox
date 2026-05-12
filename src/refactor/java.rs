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

/// Walk `target_path`'s ancestors looking for a `src/<sub>/java` triple where
/// `<sub>` is `main` or `test`. Returns the package derived from the path
/// segments BELOW the longest (nearest) matching root, so nested Maven/Gradle
/// modules under another module's `src/main/java/` resolve against the deeper
/// root. Returns `None` when no matching root is found, or `Some("")` if the
/// file sits directly under the root with no package directory.
fn derive_java_package_from_path(target_path: &Path) -> Option<String> {
    let parent = target_path.parent()?;
    let segments: Vec<String> = parent
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let n = segments.len();
    if n < 3 {
        return None;
    }
    for i in (0..=n - 3).rev() {
        if segments[i] == "src"
            && (segments[i + 1] == "main" || segments[i + 1] == "test")
            && segments[i + 2] == "java"
        {
            let pkg_parts = &segments[i + 3..];
            return Some(pkg_parts.join("."));
        }
    }
    None
}

/// Resolve the target file's package with hybrid precedence:
///   1. Explicit `target_prelude` containing `package <foo>;`.
///   2. Existing target file's `package` declaration.
///   3. Source-root-derived package from `target_path`'s filesystem location.
///   4. Source's package — only when target shares a directory with source.
///   5. Hard error — operator must pass `target_prelude` explicitly.
///
/// Returns `Ok(None)` only for the default (root) package on a `src/.../java`
/// root match with no package directory below it.
fn resolve_java_target_package(
    p: &RefactorPlanParams,
    source: &str,
    source_path: &Path,
    target_path: &Path,
) -> Result<Option<String>> {
    if let Some(prelude) = p.target_prelude.as_deref() {
        if let Some(pkg) = extract_java_package(prelude) {
            return Ok(Some(pkg));
        }
    }
    if target_path.exists() {
        if let Ok(existing) = fs::read_to_string(target_path) {
            if let Some(pkg) = extract_java_package(&existing) {
                return Ok(Some(pkg));
            }
        }
    }
    if let Some(pkg) = derive_java_package_from_path(target_path) {
        return Ok(if pkg.is_empty() { None } else { Some(pkg) });
    }
    let same_dir = match (source_path.parent(), target_path.parent()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if same_dir {
        return Ok(extract_java_package(source));
    }
    bail!(
        "cannot derive target package for {}: no `src/{{main,test}}/java` ancestor found and target directory differs from source. \
         Pass an explicit `target_prelude` with a `package <foo>;` declaration.",
        target_path.display()
    )
}

/// Build a `TextEdit` that inserts `import <fqcn>;` into a source file's
/// import block. Returns `None` if an identical import already exists.
fn java_source_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let import_line = format!("import {fqcn};");
    if source.lines().any(|line| line.trim() == import_line) {
        return None;
    }
    let mut last_import_end: Option<usize> = None;
    let mut package_end: Option<usize> = None;
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
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
    let (insert_at, replacement) = if let Some(end) = last_import_end {
        (end, format!("{import_line}\n"))
    } else if let Some(end) = package_end {
        (end, format!("\n{import_line}\n"))
    } else {
        (0, format!("{import_line}\n\n"))
    };
    Some(TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement,
    })
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
    // Gap 4: type names appearing as receivers of static method calls or
    // static member references parse as plain `identifier` nodes, not
    // `type_identifier`. Without this, the organize-imports heuristic
    // misses references like `DateUtils.getX(...)`, `Collectors.toList()`,
    // `BigDecimal.ZERO`, etc. — and either drops the source's singular
    // import for them or never adds the JDK/Vaadin import the moved body
    // needs.
    //
    // Heuristic: an identifier child of a `method_invocation` (as its
    // `object` field) or of a `field_access` (as its `object` field) is a
    // type reference when the name starts with an uppercase letter. This
    // matches Java's name-shape convention. False positives are limited
    // to local variables that violate convention; those will simply fail
    // to import (no project type matches) and be pruned later.
    if matches!(node.kind(), "method_invocation" | "field_access") {
        if let Some(receiver) = node.child_by_field_name("object") {
            if receiver.kind() == "identifier" {
                if let Ok(text) = receiver.utf8_text(source.as_bytes()) {
                    if text.chars().next().is_some_and(|c| c.is_uppercase())
                        && !java_builtin_type(text)
                    {
                        out.insert(text.to_string());
                    }
                }
            }
        }
    }
    // `method_reference` nodes (`Foo::bar`) parse with the qualifier as
    // the first named child — an `identifier` for `Foo::bar`, or
    // `scoped_identifier` / `this` / `super` / `type_identifier` for the
    // other shapes. The uppercase-initial identifier case is the same
    // convention-based type signal handled above for static-call
    // receivers; without it, `setItemLabelGenerator(EnumConverter::toLabel)`
    // moves to the extracted target with its `EnumConverter` import pruned.
    if node.kind() == "method_reference" {
        let mut cursor = node.walk();
        if let Some(qualifier) = node.named_children(&mut cursor).next() {
            if qualifier.kind() == "identifier" {
                if let Ok(text) = qualifier.utf8_text(source.as_bytes()) {
                    if text.chars().next().is_some_and(|c| c.is_uppercase())
                        && !java_builtin_type(text)
                    {
                        out.insert(text.to_string());
                    }
                }
            }
        }
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
    let Some((start, end, replacement)) =
        compute_java_organize_imports_edit(project_dir, &parsed.source, &parsed.tree)?
    else {
        return Ok(Vec::new());
    };
    Ok(vec![FileEdit {
        path: path_string(source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: vec![TextEdit {
            byte_start: start,
            byte_end: end,
            replacement,
        }],
        new_text: None,
    }])
}

/// In-memory variant of the organize-imports heuristic that operates on a
/// Java source string. Returns the rewritten source with unused imports
/// pruned and any project-local simple-name references imported. Used by
/// composite plans (e.g. `extract_java_class`) to clean up generated target
/// files before they hit disk — no LSP roundtrip, no temporary file.
///
/// Matches the same rules as `heuristic_java_organize_imports`: keeps
/// `import static …` and wildcard imports verbatim, prunes regular imports
/// whose simple name does not appear in `type_identifier` references in the
/// AST, adds project-local imports for unresolved simple names, and skips
/// inner-class simple names (gap 16). On parse failure or when no rewrite is
/// needed the input string is returned unchanged.
fn heuristic_java_organize_imports_text(
    project_dir: &Path,
    source: &str,
) -> Result<String> {
    let tree = parse_source("java", source)?;
    let Some((start, end, replacement)) =
        compute_java_organize_imports_edit(project_dir, source, &tree)?
    else {
        return Ok(source.to_string());
    };
    let mut out = String::with_capacity(source.len() + replacement.len());
    out.push_str(&source[..start]);
    out.push_str(&replacement);
    out.push_str(&source[end..]);
    Ok(out)
}

/// Shared core for the file-based and text-based organize-imports
/// heuristics. Returns `Some((byte_start, byte_end, replacement))` describing
/// the import-block rewrite, or `None` when the existing block already
/// matches the desired output (i.e. no edit needed).
fn compute_java_organize_imports_edit(
    project_dir: &Path,
    source: &str,
    tree: &Tree,
) -> Result<Option<(usize, usize, String)>> {
    let mut used_types = HashSet::new();
    collect_java_type_references(tree.root_node(), source, &mut used_types);
    let current_package = extract_java_package(source);
    let existing_imports = extract_java_imports(source);
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

    // Gap 28: drop explicit single-type imports already covered by a
    // wildcard import for the same package. The wildcard provides them, so
    // listing them again is redundant. `import static …` lines are NEVER
    // dropped (they bring members, not types — wildcards on types do not
    // cover them) and explicit imports from packages without a matching
    // wildcard are left alone.
    let wildcard_packages: HashSet<String> = imports
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            // Only TYPE wildcards qualify: `import x.y.z.*;` — not
            // `import static x.y.Z.*;` which is a member wildcard.
            if trimmed.starts_with("import static ") {
                return None;
            }
            let body = trimmed
                .strip_prefix("import ")?
                .trim_end_matches(';')
                .trim();
            body.strip_suffix(".*").map(str::to_string)
        })
        .collect();
    if !wildcard_packages.is_empty() {
        imports.retain(|line| {
            let trimmed = line.trim();
            // Keep wildcards and static imports verbatim.
            if trimmed.starts_with("import static ") || trimmed.ends_with(".*;") {
                return true;
            }
            // Explicit single-type import: drop if its package is covered.
            let Some(body) = trimmed
                .strip_prefix("import ")
                .and_then(|s| s.strip_suffix(';'))
                .map(str::trim)
            else {
                return true;
            };
            let Some((pkg, _simple)) = body.rsplit_once('.') else {
                return true;
            };
            !wildcard_packages.contains(pkg)
        });
    }

    let mut sorted = imports.into_iter().collect::<Vec<_>>();
    sorted.sort();
    let (start, end, insert_at) = java_import_block_range(source);
    let replacement = if sorted.is_empty() {
        String::new()
    } else if start == end && insert_at == 0 {
        format!("{}\n\n", sorted.join("\n"))
    } else if start == end {
        format!("\n{}\n", sorted.join("\n"))
    } else {
        sorted.join("\n")
    };
    if source[start..end] == replacement {
        return Ok(None);
    }
    Ok(Some((start, end, replacement)))
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
    if target_text
        .lines()
        .any(|line| line.trim() == import_line)
    {
        return target_text.to_string();
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
        let line_start = target_text[..pos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
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

/// Look up a Java type by simple name in the project, returning the simple
/// names of methods declared on its body (top-level interface or class).
/// Returns `None` if the type is not uniquely resolvable in the project.
fn collect_interface_declared_methods(
    project_dir: &Path,
    interface_name: &str,
) -> Option<HashSet<String>> {
    let type_paths = project_java_type_paths(project_dir);
    let path = type_paths.get(interface_name)?.as_ref()?;
    let parsed = parse_source_file(path).ok()?;
    let type_node = find_java_type_declaration_by_name(&parsed, interface_name)?;
    Some(collect_java_type_method_names(&parsed, type_node))
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
    let body = type_node
        .child_by_field_name("body")
        .unwrap_or(type_node);
    let mut names = HashSet::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let mods = collect_java_modifiers(child);
        let has_concrete_modifier = mods.iter().any(|(name, _, _)| {
            matches!(name.as_str(), "default" | "static" | "private")
        });
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
                let qualified_this = access_node.id() != node.id()
                    && access_node.kind() == "field_access";
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
                    inside.split(',').map(|p| normalize_param_type_text(p)).collect()
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
                    .filter(|m| {
                        java_method_matches_param_types(parsed, m, wanted)
                    })
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
    actual
        .iter()
        .zip(wanted_types.iter())
        .all(|(a, w)| a == w)
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
        let resolved_pkg =
            resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
        let prelude = java_default_target_prelude(p, &parsed.source, resolved_pkg.as_deref());
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
        new_text: None,
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract {} methods to {}",
            selected.len(),
            target_path.display()
        ),
        kind: "extract_java_methods".to_string(),
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
        remaining_source_constant_refs: Vec::new(),
        external_calls: dependency_report.external_calls,
        inherited_dependencies: dependency_report.inherited_dependencies,
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
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

    // Mutable-capture-with-write refusal. A capture that is (a) non-final on
    // the source, (b) not listed in `move_fields`, and (c) WRITTEN inside any
    // extracted method body cannot be promoted to a `final` constructor
    // parameter — the moved code would fail `cannot assign to final variable`.
    // Refuse the plan with an operator-actionable error pointing at the
    // exact fields to add to `move_fields` (which then routes them through
    // the rewrite_remaining_accessors / generated-setter path).
    let written_mutable_captures: Vec<String> = captured_variables
        .iter()
        .filter(|c| c.source_mutable && !c.source_static_final)
        .filter(|c| !moved_field_set.contains(c.name.as_str()))
        .filter_map(|c| {
            if extracted_methods_write_to(&parsed, &selected_methods, &c.name) {
                Some(c.name.clone())
            } else {
                None
            }
        })
        .collect();
    if !written_mutable_captures.is_empty() {
        bail!(
            "error.bad_input(code=mutable_capture_with_write): extracted method bodies write to \
             mutable source field(s) {fields:?}. Promoting them to a final constructor parameter \
             on the target would fail with `cannot assign to final variable`. Add them to \
             `move_fields` to route them through the delegate-with-generated-setter path instead.",
            fields = written_mutable_captures
        );
    }

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

    // `callback_externals`: source-class methods the operator wants threaded
    // through the target as a functional-interface callback instead of
    // surfacing as a `// FIXME: external call` marker. Classify each into
    // (Runnable / Supplier<R> / Consumer<T> / Function<T,R>) by inspecting
    // the source method's signature. 2+ arg methods are refused with a
    // dedicated bad_input code.
    let callback_specs: Vec<CallbackSpec> = {
        let names = p.callback_externals.as_deref().unwrap_or_default();
        if names.is_empty() {
            Vec::new()
        } else {
            let mut specs = Vec::new();
            for name in names {
                // The method must exist on the source class.
                let method_node = find_node(parsed.tree.root_node(), |node| {
                    node.kind() == "method_declaration"
                        && node
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                            == Some(name.as_str())
                })
                .ok_or_else(|| {
                    anyhow!(
                        "error.bad_input(code=callback_method_not_found): \
                         `callback_externals` entry `{name}` is not a method declaration on the \
                         source class"
                    )
                })?;
                specs.push(classify_callback_method(&parsed, method_node)?);
            }
            specs
        }
    };
    // Append callback ctor params after instance captures so the wiring's
    // positional arg list is (captured1, captured2, ..., callback1, ...).
    let mut all_target_ctor_params = dependency_params.clone();
    for spec in &callback_specs {
        all_target_ctor_params.push(JavaParameterSpec {
            type_name: spec.interface_type.clone(),
            name: spec.field_name.clone(),
        });
    }

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
    // Gap 2: derive the target package via the unified resolver. Precedence is
    // target_prelude > existing target file's package > source-root-derived from
    // target_path > source's package (same-directory targets only) > hard error.
    // This avoids silent path↔package contradictions on cross-package extracts.
    let target_package =
        resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let cross_package = match (source_package.as_deref(), target_package.as_deref()) {
        (Some(src), Some(tgt)) => src != tgt,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
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
            extract_field_text_with_visibility_floor(&parsed, field, visibility_floor)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dependency_field_text = all_target_ctor_params
        .iter()
        .map(|param| format!("    private final {} {};", param.type_name, param.name))
        .collect::<Vec<_>>()
        .join("\n");
    let constructor_text = if all_target_ctor_params.is_empty() {
        String::new()
    } else {
        java_constructor_decl(&target_class_name, "public", &all_target_ctor_params, true, None)?
    };

    // Gap 18 + Gap 6: generate getter/setter methods on the target for each
    // moved field, plus rewrite remaining source-side reads/writes through
    // the delegate.
    //
    // Gap 6 decouples this from `deep_analysis`. Previously the accessor
    // rewrite required `deep_analysis=true` AND `rewrite_remaining_accessors`
    // unset/true; with `deep_analysis=false` a `move_fields` extract would
    // silently miscompile (the field declarations move but every read/write
    // in the source class stays as a bare reference). The accessor rewriter
    // doesn't need the full call-graph walk — it only needs to know "this
    // field was moved." So we now default `rewrite_remaining=true` whenever
    // `move_fields` is non-empty, regardless of `deep_analysis`.
    //
    // Operator opt-out: `rewrite_remaining_accessors=false` (preserves the
    // legacy report-only behavior under `deep_analysis=true`, and disables
    // the new always-on rewrite under `deep_analysis=false`).
    //
    // Generated accessors honour the same visibility floor used for the
    // moved methods (`package` same-package, `public` cross-package).
    let rewrite_remaining = !selected_fields.is_empty()
        && p.rewrite_remaining_accessors.unwrap_or(true);
    let accessor_specs: Vec<DelegateAccessorSpec> = if rewrite_remaining {
        selected_fields
            .iter()
            .map(DelegateAccessorSpec::from_field)
            .collect()
    } else {
        Vec::new()
    };
    let accessor_visibility = if cross_package { "public" } else { "package" };
    let accessor_text = if accessor_specs.is_empty() {
        String::new()
    } else {
        accessor_specs
            .iter()
            .map(|spec| render_delegate_accessors(spec, accessor_visibility))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let target_body = [
        moved_constants_text,
        dependency_field_text,
        moved_field_text,
        constructor_text,
        method_text,
        accessor_text,
    ]
    .into_iter()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let prelude = java_default_target_prelude(p, &parsed.source, target_package.as_deref());
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    if !original_target_bytes.is_empty() {
        bail!("extract_java_class currently requires a missing or empty target file");
    }
    let mut raw_target_content = java_class_wrapper(&target_class_name, &prelude, &target_body);

    // `callback_externals`: rewrite each callback invocation inside the
    // extracted method bodies BEFORE organize_imports + FIXME insertion run.
    // After rewrite, calls like `refreshGrid()` become `refreshGrid.run()`,
    // so the import-walker sees them as `Runnable`-typed field accesses and
    // the external-call FIXME finder no longer matches them by method name.
    if !callback_specs.is_empty() {
        let callback_edits =
            compute_callback_call_rewrites(&raw_target_content, &callback_specs)?;
        raw_target_content = apply_text_edits(&raw_target_content, &callback_edits);
    }

    // Gap 25: prune unused imports on the generated target. The prelude
    // copies every import from the source; the heuristic walks the in-memory
    // target AST and drops imports whose simple name is never referenced as
    // a `type_identifier`. Best-effort — on any failure we keep the over-
    // imported content rather than fail the whole extraction.
    let project_dir_for_imports = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| target_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut target_content =
        match heuristic_java_organize_imports_text(&project_dir_for_imports, &raw_target_content) {
            Ok(pruned) => pruned,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "extract_java_class: heuristic_java_organize_imports_text failed; \
                     keeping unpruned target imports"
                );
                raw_target_content
            }
        };
    // Inject any functional-interface imports needed by the callbacks
    // (Runnable lives in java.lang; Consumer/Supplier/Function live under
    // java.util.function and need an explicit import even though their
    // simple name is referenced by the ctor parameter type).
    for spec in &callback_specs {
        if let Some(fqcn) = spec.extra_import.as_deref() {
            target_content = java_inject_import(&target_content, fqcn);
        }
    }

    // Gap 22 / Gap 23: scaffold unresolved deps in the generated target text.
    // Only meaningful when deep_analysis was on (the report is empty otherwise).
    if p.deep_analysis.unwrap_or(false) {
        // Collect extracted method names (the methods that just moved to the
        // target). Used to test interface satisfaction for Gap 23.
        let extracted_method_names: HashSet<String> = selected_methods
            .iter()
            .filter_map(|m| m.item.name.clone())
            .collect();

        // Gap 23: pick interfaces to inject into the target's class declaration.
        //
        // Triggers (union):
        //   1. Any interface that appears in `inherited_dependencies` (an
        //      extracted method called a method declared on the interface).
        //   2. Any interface from the source class's `implements` clause
        //      whose declared methods are all present in the extracted set
        //      — the source's interface contract migrated wholesale to the
        //      target. This case can NOT surface in `inherited_dependencies`
        //      because the analyzer drops calls to extracted methods, so the
        //      `implements`-list scan is the only way to spot it.
        let project_dir_path = p.project_dir.as_deref().map(Path::new);
        let mut implements_to_add: Vec<String> = Vec::new();
        let mut interface_imports: Vec<String> = Vec::new();
        let mut unsatisfied_interfaces: Vec<(String, Vec<String>)> = Vec::new();
        let mut interface_sources: BTreeMap<String, ()> = BTreeMap::new();
        for dep in &class_dependency_report.inherited_dependencies {
            if dep.source_kind == "interface" {
                interface_sources.insert(dep.source.clone(), ());
            }
        }
        // Trigger (2): scan source class's `implements` chain for interfaces
        // whose method set is entirely extracted.
        if let Some(project_dir) = project_dir_path {
            for super_name in collect_java_super_type_names(class_node, &parsed.source) {
                // Skip types we already have via inherited_dependencies.
                if interface_sources.contains_key(&super_name) {
                    continue;
                }
                let type_paths = project_java_type_paths(project_dir);
                let Some(Some(path)) = type_paths.get(&super_name) else {
                    continue;
                };
                let Ok(parsed_super) = parse_source_file(path) else {
                    continue;
                };
                let Some(super_node) = find_java_type_declaration_by_name(&parsed_super, &super_name)
                else {
                    continue;
                };
                if java_type_kind_label(super_node) != "interface" {
                    continue;
                }
                let declared = collect_java_type_method_names(&parsed_super, super_node);
                if declared.is_empty() {
                    continue;
                }
                // All declared methods must be in the extracted set.
                let all_satisfied = declared
                    .iter()
                    .all(|m| extracted_method_names.contains(m));
                if all_satisfied {
                    interface_sources.insert(super_name, ());
                }
            }
        }
        for interface_name in interface_sources.keys() {
            // Need a project_dir to resolve the interface for both the type
            // index and the method-list lookup.
            let Some(project_dir) = project_dir_path else {
                continue;
            };
            // Gather the interface's ABSTRACT methods only — methods the
            // implementer must explicitly provide. `default`, `static`,
            // and `private` interface methods already have bodies on the
            // interface itself and don't need to be in the extracted set.
            // If the interface is not uniquely resolvable in the project,
            // skip it — we can't safely add `implements` for a type we
            // can't import.
            let Some(declared_methods) =
                collect_interface_abstract_method_names(project_dir, interface_name)
            else {
                continue;
            };
            // Satisfaction: every abstract method on the interface must be
            // in the extracted method set. Else the target is abstract.
            let unsatisfied: Vec<String> = declared_methods
                .iter()
                .filter(|m| !extracted_method_names.contains(m.as_str()))
                .cloned()
                .collect();
            // Look up FQCN to add a matching import when the interface lives
            // in a different package.
            let type_index = build_java_type_index(project_dir).ok();
            let fqcn_opt = type_index
                .as_ref()
                .and_then(|idx| idx.top_level.get(interface_name))
                .and_then(|slot| slot.clone());
            if let Some(fqcn) = fqcn_opt.as_ref() {
                let target_pkg = extract_java_package(&target_content);
                let same_pkg = target_pkg
                    .as_deref()
                    .is_some_and(|pkg| fqcn.strip_suffix(&format!(".{interface_name}")) == Some(pkg));
                if !same_pkg {
                    interface_imports.push(fqcn.clone());
                }
            }
            implements_to_add.push(interface_name.clone());
            if !unsatisfied.is_empty() {
                unsatisfied_interfaces.push((interface_name.clone(), unsatisfied));
            }
        }
        for fqcn in &interface_imports {
            target_content = java_inject_import(&target_content, fqcn);
        }
        if !implements_to_add.is_empty() {
            target_content =
                java_inject_implements(&target_content, &target_class_name, &implements_to_add);
        }
        // Insert FIXME marker above the class declaration for any
        // implements that the target cannot satisfy.
        if !unsatisfied_interfaces.is_empty() {
            let needle = format!("public class {target_class_name}");
            if let Some(decl_at) = target_content.find(&needle) {
                let line_start = target_content[..decl_at]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let indent: String = target_content[line_start..decl_at]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let mut comment = String::new();
                for (interface, missing) in &unsatisfied_interfaces {
                    let names = missing.join(", ");
                    comment.push_str(&format!(
                        "{indent}// FIXME: target now implements {interface} but does not satisfy method(s) <{names}>;\n"
                    ));
                    comment.push_str(&format!(
                        "{indent}// either also extract the listed method(s) or remove the implements clause.\n"
                    ));
                }
                let mut patched = String::with_capacity(target_content.len() + comment.len());
                patched.push_str(&target_content[..line_start]);
                patched.push_str(&comment);
                patched.push_str(&target_content[line_start..]);
                target_content = patched;
            }
        }

        // Gap 22: insert FIXME comment lines above each external_call site
        // in the generated target body. `callback_externals` methods are
        // already threaded through the target as functional-interface
        // callbacks (call sites rewritten to `field.run()` / `.accept(...)`
        // etc.); they are NOT external calls that need a FIXME.
        let callback_names: HashSet<&str> = callback_specs
            .iter()
            .map(|c| c.method_name.as_str())
            .collect();
        for call in &class_dependency_report.external_calls {
            if callback_names.contains(call.method.as_str()) {
                continue;
            }
            let fixme = fixme_external_call(&call.method);
            let (next, _count) =
                java_insert_fixme_above_calls(&target_content, &call.method, &fixme);
            target_content = next;
        }

        // Gap 23 (class branch): for inherited deps with source_kind=="class",
        // do NOT auto-add `extends`. Insert FIXMEs above each call site.
        for dep in &class_dependency_report.inherited_dependencies {
            if dep.source_kind != "class" {
                continue;
            }
            let fixme = fixme_inherited_class_call(&dep.method, &dep.source);
            let (next, _count) =
                java_insert_fixme_above_calls(&target_content, &dep.method, &fixme);
            target_content = next;
        }

        // Gap 29: warn the operator about mutable captures that were
        // promoted to `final` constructor params on the target. The source
        // field is non-final (its value can change at runtime); the target
        // only sees a snapshot taken at construction time. This is a silent
        // semantic bug — surface it as a FIXME directly above the
        // generated `private final <Type> <name>;` line so it travels with
        // the target file rather than getting buried in JSON.
        for capture in &captured_variables {
            if !capture.source_mutable {
                continue;
            }
            if capture.source_static_final {
                // Static-finals route through the Gap 20 constants path —
                // they aren't constructor params and can't be "snapshotted".
                continue;
            }
            if moved_field_set.contains(capture.name.as_str()) {
                // Field is being moved, not captured as a param.
                continue;
            }
            target_content =
                java_insert_fixme_above_mutable_capture(&target_content, capture);
        }
    }

    // Source-class inner type references (Gap 5). Methods being moved may
    // reference an inner type (enum / class / record / interface) declared
    // on the source class — bare names that resolve from the source's
    // package but won't resolve from the target's. Rewrite each such
    // reference in the target body to `<SourceClass>.<InnerType>`, add
    // a source-class import on the target (cross-package only), and widen
    // the inner type's visibility on the source to at least the same
    // floor the planner applies to moved methods.
    let inner_type_decls: BTreeMap<String, Node<'_>> = {
        let mut map = BTreeMap::new();
        if let Some(class_body) = class_node.child_by_field_name("body") {
            let mut cursor = class_body.walk();
            for child in class_body.named_children(&mut cursor) {
                let kind = child.kind();
                if matches!(
                    kind,
                    "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                ) {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                            map.insert(name.to_string(), child);
                        }
                    }
                }
            }
        }
        map
    };
    let source_class_name = java_class_name(class_node, &parsed.source);
    let mut referenced_inner_types: BTreeSet<String> = BTreeSet::new();
    if !inner_type_decls.is_empty() {
        // Walk the assembled target text for type_identifier references
        // matching an inner-type name. Skip references already qualified
        // (scoped_type_identifier — the operator wrote `Outer.Inner`
        // themselves) and the inner-type declarations the target may have
        // itself (defensive — they shouldn't appear, but if a moved cluster
        // somehow brought one along, don't qualify the declaration site).
        if let Ok(target_tree) = parse_source("java", &target_content) {
            let mut edits: Vec<(usize, usize, String)> = Vec::new();
            let mut stack = vec![target_tree.root_node()];
            while let Some(node) = stack.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack.push(ch);
                }
                if node.kind() != "type_identifier" {
                    continue;
                }
                let Ok(text) = node.utf8_text(target_content.as_bytes()) else {
                    continue;
                };
                if !inner_type_decls.contains_key(text) {
                    continue;
                }
                // Skip references that are already part of a
                // `Outer.Inner` qualified type. tree-sitter-java exposes
                // that as `scoped_type_identifier` containing two
                // `type_identifier` children; if our match is one of
                // them, the parent's kind tells us.
                if node
                    .parent()
                    .map(|p| p.kind() == "scoped_type_identifier")
                    .unwrap_or(false)
                {
                    continue;
                }
                edits.push((
                    node.start_byte(),
                    node.end_byte(),
                    format!("{source_class_name}.{text}"),
                ));
                referenced_inner_types.insert(text.to_string());
            }
            // Also pick up uppercase-initial identifier receivers of
            // method_invocation / field_access that match an inner enum
            // name (`InnerEnum.VALUE`).
            let mut stack2 = vec![target_tree.root_node()];
            while let Some(node) = stack2.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack2.push(ch);
                }
                if !matches!(node.kind(), "method_invocation" | "field_access") {
                    continue;
                }
                let Some(receiver) = node.child_by_field_name("object") else {
                    continue;
                };
                if receiver.kind() != "identifier" {
                    continue;
                }
                let Ok(text) = receiver.utf8_text(target_content.as_bytes()) else {
                    continue;
                };
                if !inner_type_decls.contains_key(text) {
                    continue;
                }
                edits.push((
                    receiver.start_byte(),
                    receiver.end_byte(),
                    format!("{source_class_name}.{text}"),
                ));
                referenced_inner_types.insert(text.to_string());
            }
            edits.sort_by_key(|e| e.0);
            // Dedupe overlapping edits (e.g., a type_identifier inside a
            // method_invocation receiver matched twice).
            edits.dedup_by_key(|e| e.0);
            target_content = apply_text_edits(&target_content, &edits);
        }
        // Cross-package: target needs to import the source class so the
        // qualified `<SourceClass>.<InnerType>` references resolve.
        if !referenced_inner_types.is_empty() && cross_package {
            if let Some(src_pkg) = source_package.as_deref() {
                let source_fqcn = format!("{src_pkg}.{source_class_name}");
                target_content = java_inject_import(&target_content, &source_fqcn);
            }
        }
    }

    // Cross-package bare-field-to-getter rewrites (Gap 6). When the moved
    // code accesses a `private` field of a source-class inner type via a
    // method parameter (e.g. `truckTicket.direction`), the bare access
    // fails to compile from a different package. The inner type often
    // already declares a public getter (`getDirection()`); rewrite the
    // bare access to the getter call so cross-package extracts compile
    // without manual fixup. Same-package extracts retain bare access.
    if cross_package && !inner_type_decls.is_empty() {
        let mut field_to_getter_by_type: BTreeMap<String, BTreeMap<String, String>> =
            BTreeMap::new();
        for (type_name, type_node) in &inner_type_decls {
            let Some(type_body) = type_node.child_by_field_name("body") else { continue };
            let mut fields: Vec<(String, String)> = Vec::new();
            let mut getters: BTreeSet<String> = BTreeSet::new();
            let mut tcur = type_body.walk();
            for child in type_body.named_children(&mut tcur) {
                match child.kind() {
                    "field_declaration" => {
                        if let (Some(n), Some(ty)) = (
                            java_field_declaration_name(child, &parsed.source),
                            java_field_type_text(child, &parsed.source),
                        ) {
                            fields.push((n, ty));
                        }
                    }
                    "method_declaration" => {
                        let mods = collect_java_modifiers(child);
                        let is_public = mods.iter().any(|(m, _, _)| m == "public");
                        if !is_public {
                            continue;
                        }
                        let no_params = child
                            .child_by_field_name("parameters")
                            .map(|p| {
                                let mut pcur = p.walk();
                                !p.named_children(&mut pcur)
                                    .any(|n| n.kind() == "formal_parameter")
                            })
                            .unwrap_or(false);
                        if !no_params {
                            continue;
                        }
                        if let Some(name_node) = child.child_by_field_name("name") {
                            if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                                getters.insert(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut getter_map: BTreeMap<String, String> = BTreeMap::new();
            for (field_name, field_type) in fields {
                let cap = {
                    let mut chars = field_name.chars();
                    match chars.next() {
                        Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                        None => String::new(),
                    }
                };
                let mut candidates = vec![format!("get{cap}")];
                if field_type.trim() == "boolean" {
                    candidates.insert(0, format!("is{cap}"));
                }
                for candidate in candidates {
                    if getters.contains(&candidate) {
                        getter_map.insert(field_name.clone(), candidate);
                        break;
                    }
                }
            }
            if !getter_map.is_empty() {
                field_to_getter_by_type.insert(type_name.clone(), getter_map);
            }
        }

        if !field_to_getter_by_type.is_empty() {
            // Re-parse target_content (post Gap-5 inner-type qualification)
            // and walk for `field_access` whose receiver is a parameter
            // typed as one of these inner types.
            if let Ok(target_tree) = parse_source("java", &target_content) {
                let mut edits: Vec<(usize, usize, String)> = Vec::new();
                let mut tstack = vec![target_tree.root_node()];
                while let Some(tnode) = tstack.pop() {
                    let mut c = tnode.walk();
                    for ch in tnode.named_children(&mut c) {
                        tstack.push(ch);
                    }
                    if tnode.kind() != "method_declaration"
                        && tnode.kind() != "constructor_declaration"
                    {
                        continue;
                    }
                    // Map parameter name → inner-type simple name.
                    let mut typed_receivers: BTreeMap<String, String> = BTreeMap::new();
                    if let Some(params) = tnode.child_by_field_name("parameters") {
                        let mut pc = params.walk();
                        for p in params.named_children(&mut pc) {
                            if p.kind() != "formal_parameter" {
                                continue;
                            }
                            let ty = p
                                .child_by_field_name("type")
                                .and_then(|t| t.utf8_text(target_content.as_bytes()).ok())
                                .map(|s| s.trim().to_string());
                            let name = p
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(target_content.as_bytes()).ok())
                                .map(|s| s.to_string());
                            if let (Some(ty), Some(name)) = (ty, name) {
                                // Param type may be `Outer.Inner` after the
                                // Gap-5 qualification pass; strip the
                                // qualifier to look up by simple name.
                                let simple = ty
                                    .rsplit_once('.')
                                    .map(|(_, s)| s.trim().to_string())
                                    .unwrap_or_else(|| ty.trim().to_string());
                                if field_to_getter_by_type.contains_key(&simple) {
                                    typed_receivers.insert(name, simple);
                                }
                            }
                        }
                    }
                    if typed_receivers.is_empty() {
                        continue;
                    }
                    // Walk this method's body for field_access on those
                    // receivers.
                    let Some(body) = tnode.child_by_field_name("body") else { continue };
                    let mut bstack = vec![body];
                    while let Some(node) = bstack.pop() {
                        let mut bc = node.walk();
                        for ch in node.named_children(&mut bc) {
                            bstack.push(ch);
                        }
                        if node.kind() != "field_access" {
                            continue;
                        }
                        let Some(obj) = node.child_by_field_name("object") else { continue };
                        if obj.kind() != "identifier" {
                            continue;
                        }
                        let Ok(obj_name) = obj.utf8_text(target_content.as_bytes()) else { continue };
                        let Some(ty) = typed_receivers.get(obj_name) else { continue };
                        let Some(field_node) = node.child_by_field_name("field") else { continue };
                        let Ok(field_name) = field_node.utf8_text(target_content.as_bytes()) else { continue };
                        let Some(getter_map) = field_to_getter_by_type.get(ty) else { continue };
                        let Some(getter) = getter_map.get(field_name) else { continue };
                        edits.push((
                            node.start_byte(),
                            node.end_byte(),
                            format!("{obj_name}.{getter}()"),
                        ));
                    }
                }
                edits.sort_by_key(|e| e.0);
                edits.dedup_by_key(|e| e.0);
                target_content = apply_text_edits(&target_content, &edits);
            }
        }
    }

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

    // Source-side visibility widening for referenced inner types (Gap 5).
    // Widen each below-floor inner-type declaration to the floor so the
    // qualified `<SourceClass>.<InnerType>` references on the new target
    // can reach them. Inner types already at/above the floor stay
    // unchanged.
    for name in &referenced_inner_types {
        if let Some(inner_node) = inner_type_decls.get(name) {
            let mods = collect_java_modifiers(*inner_node);
            let current = java_visibility_from_mods(&mods);
            if java_visibility_rank(current) >= java_visibility_rank(visibility_floor) {
                continue;
            }
            let new_visibility = if visibility_floor == "package" {
                None
            } else {
                Some(visibility_floor)
            };
            let vis_edit =
                build_visibility_rewrite_edit(*inner_node, &mods, new_visibility, &parsed.source);
            source_edits.push(vis_edit);
        }
    }

    let field_insert_at = java_class_body_insert_position(class_node, &parsed.source);
    let delegate_edit_idx = source_edits.len();
    source_edits.push(TextEdit {
        byte_start: field_insert_at,
        byte_end: field_insert_at,
        replacement: format!("\n    private final {target_class_name} {delegate_field};"),
    });
    // Gap 5: when the target type lands in a different package than the source,
    // the source needs `import <target_pkg>.<TargetClass>;` so the new delegate
    // field declaration resolves. Same-package targets resolve implicitly.
    if cross_package {
        if let Some(target_pkg) = target_package.as_deref() {
            let fqcn = if target_pkg.is_empty() {
                target_class_name.clone()
            } else {
                format!("{target_pkg}.{target_class_name}")
            };
            if let Some(import_edit) = java_source_import_edit(&parsed.source, &fqcn) {
                source_edits.push(import_edit);
            }
        }
    }
    // Source-side wiring args: capture names verbatim, then `this::<method>`
    // for each callback (the target's ctor takes the captures first, then the
    // functional-interface callbacks, in declaration order).
    let assignment = {
        let mut args: Vec<String> = dependency_params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        for spec in &callback_specs {
            args.push(format!("this::{}", spec.method_name));
        }
        format!(
            "this.{delegate_field} = new {target_class_name}({});",
            args.join(", ")
        )
    };
    // Track the wiring edit so we can post-process its position after the
    // accessor-rewrite pass (Gap 8: topo-sort wiring against accessor edits
    // that introduce `<delegate_field>.<getter>()` reads earlier in the
    // same ctor body — the wiring must come before any such read).
    let mut wiring_state: Option<WiringInsertState> = None;
    if let Some(constructor) = first_constructor_node(class_node, &parsed.source) {
        // Gap 7: when any captured-param name refers to a field rather than
        // a constructor parameter, that name is `null` until its own
        // `this.field = ...` assignment runs. Inserting the wiring as the
        // FIRST body statement evaluates the field reads too early — for
        // `final` fields the compiler rejects it (`might not have been
        // initialized`); for non-final fields it silently captures null.
        //
        // Defer the wiring statement until after the last source-ctor
        // assignment to any of those field-only captures. When every
        // captured name is also a ctor parameter (in scope from the
        // parameter list), insertion at the body start is safe and matches
        // the legacy placement.
        let ctor_params = constructor_parameter_names(constructor, &parsed.source);
        let field_only_captures: HashSet<&str> = dependency_params
            .iter()
            .map(|p| p.name.as_str())
            .filter(|name| !ctor_params.contains(*name))
            .collect();
        let lower_bound = if field_only_captures.is_empty() {
            constructor_body_insert_position(constructor, &parsed.source)
        } else {
            match last_field_assign_end_in_constructor(
                constructor,
                &parsed.source,
                &field_only_captures,
            ) {
                Some(end) => end,
                None => constructor_body_insert_position(constructor, &parsed.source),
            }
        };
        let body_range = constructor
            .child_by_field_name("body")
            .map(|b| (b.start_byte(), b.end_byte()));
        let edit_idx = source_edits.len();
        source_edits.push(TextEdit {
            byte_start: lower_bound,
            byte_end: lower_bound,
            replacement: format!("\n        {assignment}"),
        });
        wiring_state = Some(WiringInsertState {
            edit_idx,
            body_range,
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
    // Compute caller rewrites separately so they can be threaded into the
    // accessor-rewrite pass below (Gap 1: caller-rewrite zero-width inserts
    // inside an LHS-write RHS must be absorbed into the LHS-write rendering,
    // not added to the global edit list).
    let caller_edits = java_caller_rewrite_edits(
        &parsed,
        method_names,
        delegate_field,
        &removed_ranges,
    )?;

    // Gap 18 + Gap 6: rewrite each remaining source-side read/write of a
    // moved field through the delegate's generated getter/setter. Now fires
    // whenever `move_fields` is non-empty (regardless of `deep_analysis`)
    // unless the operator explicitly passes `rewrite_remaining_accessors=false`.
    if rewrite_remaining && !accessor_specs.is_empty() {
        // Skip ranges include both the moved field declarations AND the
        // moved method bodies — accesses inside moved methods are about to
        // be deleted as part of the extraction, so rewriting them would
        // overlap with the deletion edit.
        let mut skip_ranges: Vec<(usize, usize)> = selected_fields
            .iter()
            .map(|field| (field.item.byte_start, field.item.byte_end))
            .collect();
        skip_ranges.extend(
            selected_methods
                .iter()
                .map(|method| (method.item.byte_start, method.item.byte_end)),
        );
        let (accessor_rewrite_edits, residual_caller_edits) =
            compute_remaining_accessor_rewrite_edits(
                &parsed,
                &accessor_specs,
                &skip_ranges,
                delegate_field,
                caller_edits,
            )?;
        source_edits.extend(residual_caller_edits);
        source_edits.extend(accessor_rewrite_edits);
    } else {
        source_edits.extend(caller_edits);
    }

    // Gap 8: surface a stacked-extract ordering conflict when an accessor
    // rewrite earlier in the same ctor body would read `<delegate_field>`
    // before the planned wiring assigns it (compile-time
    // `variable <name> might not have been initialized`).
    //
    // We DON'T auto-rewrite: the only safe move would be pulling wiring
    // back past its field-only-capture lower bound (Gap 7's invariant),
    // which trades the compile error for a silent null-capture of those
    // captures. A proper fix needs to also relocate the field-only-capture
    // assignments themselves, which is invasive. For v1 the planner just
    // emits a tracing warning with diagnostics — the operator then either
    // (a) swaps statements manually, or (b) re-orders the extract sequence
    // so the delegate field exists before its consumers in the source ctor.
    if let Some(state) = wiring_state.as_ref() {
        if let Some((body_start, body_end)) = state.body_range {
            let wiring_pos = source_edits[state.edit_idx].byte_start;
            let earliest_accessor_in_ctor = source_edits
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx != state.edit_idx)
                .map(|(_, e)| e)
                .filter(|e| e.byte_start >= body_start && e.byte_end <= body_end)
                .map(|e| e.byte_start)
                .filter(|pos| *pos < wiring_pos)
                .min();
            if let Some(min_pos) = earliest_accessor_in_ctor {
                let (acc_line, _) = line_col(&parsed.source, min_pos);
                let (wir_line, _) = line_col(&parsed.source, wiring_pos);
                tracing::warn!(
                    target = "refactor",
                    kind = "extract_java_class",
                    code = "ctor_wiring_ordering_conflict",
                    delegate_field = %delegate_field,
                    wiring_line = wir_line,
                    accessor_line = acc_line,
                    "stacked-extract: delegate `{}` is read by an accessor \
                     rewrite at line {} but its wiring assignment lands at line {} — \
                     the post-apply ctor will fail to compile with \
                     `variable {} might not have been initialized`. Swap the \
                     two statements manually after apply.",
                    delegate_field,
                    acc_line,
                    wir_line,
                    delegate_field,
                );
            }
        }
    }
    let _ = wiring_state; // suppress unused-binding warning when no rewrites land

    // Constants whose declaration is moving lose their bare-name binding in
    // the source. Rewrite every remaining source-side reference (outside the
    // extracted methods and outside the moved declarations) to a qualified
    // `<TargetClass>.<CONST>` form. Constants are also rendered on the target
    // with a widened visibility (handled above by
    // `extract_field_text_with_visibility_floor`), so the qualified
    // references resolve from a different package when cross_package.
    let constant_names: Vec<String> = moved_constant_fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    if !constant_names.is_empty() {
        let mut const_skip_ranges: Vec<(usize, usize)> = selected_methods
            .iter()
            .map(|method| (method.item.byte_start, method.item.byte_end))
            .collect();
        const_skip_ranges.extend(
            selected_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end)),
        );
        const_skip_ranges.extend(
            moved_constant_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end)),
        );
        source_edits.extend(compute_remaining_constant_qualify_edits(
            &parsed,
            &constant_names,
            &const_skip_ranges,
            &target_class_name,
        ));
    }

    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Gap 26: when fields are moved AND deep_analysis is on, surface every
    // remaining read/write of the moved fields that still lives in the
    // source class. Mirrors `plan_move_java_field`'s contract — operators
    // get one shape across both plan kinds.
    let remaining_source_accessors =
        if !selected_fields.is_empty() && p.deep_analysis.unwrap_or(false) {
            // Skip ranges include both the moved field declarations (about
            // to be deleted from source) AND the bodies of every method in
            // `item_names` (those methods move to the target, so accesses
            // inside them are about to leave the source class entirely —
            // listing them as "remaining" is a false positive).
            let mut skip_ranges = selected_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end))
                .collect::<Vec<_>>();
            skip_ranges.extend(
                selected_methods
                    .iter()
                    .map(|method| (method.item.byte_start, method.item.byte_end)),
            );
            let moved_field_names_owned = selected_fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>();
            compute_remaining_source_accessors(
                &parsed,
                &moved_field_names_owned,
                &skip_ranges,
            )
        } else {
            Vec::new()
        };

    // Preview report for moved constants — populated under deep_analysis
    // analogous to `remaining_source_accessors`. The qualifier rewrites
    // themselves run unconditionally (see above) so cross-cluster refs
    // never silently miscompile.
    let remaining_source_constant_refs =
        if !constant_names.is_empty() && p.deep_analysis.unwrap_or(false) {
            let mut skip = selected_methods
                .iter()
                .map(|m| (m.item.byte_start, m.item.byte_end))
                .collect::<Vec<_>>();
            skip.extend(
                selected_fields
                    .iter()
                    .map(|f| (f.item.byte_start, f.item.byte_end)),
            );
            skip.extend(
                moved_constant_fields
                    .iter()
                    .map(|f| (f.item.byte_start, f.item.byte_end)),
            );
            compute_remaining_source_constant_refs(&parsed, &constant_names, &skip)
        } else {
            Vec::new()
        };

    // Gap 4: cross-file caller rewrites for moved static members. Every
    // `OldClass.<static>` reference in other project files gets its
    // qualifier rewritten to `NewClass`. Only static methods + moved
    // constants qualify here — instance methods on the source class still
    // resolve through the source-side private delegate field, which is
    // unreachable from other files (a future iteration could surface a
    // forwarding-method advisory for cross-file instance callers).
    let mut moved_static_items: Vec<MovedStaticItem> = Vec::new();
    for method in &selected_methods {
        let Some(method_name) = method.item.name.as_deref() else {
            continue;
        };
        let method_node = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        });
        if let Some(node) = method_node {
            if method_is_static(node) {
                moved_static_items.push(MovedStaticItem {
                    name: method_name.to_string(),
                    kind: "method",
                });
            }
        }
    }
    // moved_constant_fields covers static-final captures of extracted
    // methods (the Gap-20 path). selected_fields covers explicit
    // `move_fields` entries — when an operator moves a constant via
    // move_fields without it being captured, it lands here. Both
    // populate the cross-file rewrite list.
    for field in &moved_constant_fields {
        moved_static_items.push(MovedStaticItem {
            name: field.name.clone(),
            kind: "field",
        });
    }
    for field in &selected_fields {
        // Already covered by moved_constant_fields?
        if moved_constant_fields.iter().any(|f| f.name == field.name) {
            continue;
        }
        let field_node = find_node(parsed.tree.root_node(), |node| {
            node.kind() == "field_declaration"
                && node.start_byte() == field.item.byte_start
                && node.end_byte() == field.item.byte_end
        });
        if let Some(node) = field_node {
            if has_java_modifier(node, "static") && has_java_modifier(node, "final") {
                moved_static_items.push(MovedStaticItem {
                    name: field.name.clone(),
                    kind: "field",
                });
            }
        }
    }
    let cross_file_edits = if let Some(project_dir) = p.project_dir.as_deref() {
        compute_cross_file_static_caller_edits(
            Path::new(project_dir),
            &source_path,
            &target_path,
            &source_class_name,
            &target_class_name,
            target_package.as_deref(),
            &moved_static_items,
        )
    } else {
        Vec::new()
    };

    let mut all_edits = vec![
        FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: source_edits,
            new_text: None,
        },
        FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(&original_target_bytes),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: original_target_bytes.len(),
                replacement: target_content,
            }],
            new_text: None,
        },
    ];
    let mut all_validations: Vec<ValidationStep> = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect();
    for fe in &cross_file_edits {
        let caller_path = PathBuf::from(&fe.path);
        all_validations.extend(parse_validation_step_for_path(&caller_path));
    }
    all_edits.extend(cross_file_edits);

    let plan = RefactorPlan {
        title: format!(
            "Extract Java class {} from {}",
            target_class_name,
            source_path.display()
        ),
        kind: "extract_java_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: all_edits,
        validations: all_validations,
        items: selected_methods
            .into_iter()
            .map(|method| method.item)
            .chain(selected_fields.into_iter().map(|field| field.item))
            .collect(),
        leftovers: Vec::new(),
        captured_variables,
        remaining_source_accessors,
        remaining_source_constant_refs,
        external_calls: class_dependency_report.external_calls,
        inherited_dependencies: class_dependency_report.inherited_dependencies,
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Capture-aware promotion of a non-static inner class to a top-level class.
///
/// See `sm-refactor-java` "What `promote_java_inner_class` Does" for the full
/// contract. Briefly: analyzes outer-field reads inside the inner class,
/// synthesizes (or augments) a constructor with those captures as `final`
/// parameters, rewrites every `new <Inner>(...)` site in the source to pass
/// the captured values, drops the inner declaration from source, and emits
/// the new top-level class in its own file.
///
/// Refuses many edge cases up-front rather than emitting broken Java:
/// - `static_inner_class_in_promote` — use `extract_java_nested_classes`.
/// - `inner_class_writes_outer_field` — `final` ctor params can't be assigned.
/// - `inner_class_calls_outer_method` — needs callback threading; v2.
/// - `inner_class_multiple_ctors` / `inner_class_this_chain_ctor` —
///   definite-assignment of captures across paths is non-trivial.
/// - `inner_class_referenced_as_type` — only `new <Inner>(...)` is
///   rewritten in v1; other type-position uses need manual handling.
pub(crate) fn plan_promote_java_inner_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for promote_java_inner_class"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("promote_java_inner_class only supports java files");
    }

    let inner_name: String = p
        .item_names
        .as_deref()
        .and_then(|v| v.first().cloned())
        .or_else(|| p.module_name.clone())
        .ok_or_else(|| {
            anyhow!(
                "promote_java_inner_class requires the inner class name in either \
                 `item_names` (single entry) or `module_name`"
            )
        })?;
    validate_java_type_identifier(&inner_name, "inner class name")?;

    let outer_class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no outer class declaration found in {}", source_path.display()))?;
    let outer_body = outer_class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("outer class has no body"))?;
    let inner_class_node = {
        let mut cursor = outer_body.walk();
        outer_body
            .named_children(&mut cursor)
            .find(|child| {
                matches!(
                    child.kind(),
                    "class_declaration" | "record_declaration"
                ) && child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                    == Some(inner_name.as_str())
            })
            .ok_or_else(|| {
                anyhow!(
                    "inner class `{inner_name}` not found as a direct member of {}",
                    java_class_name(outer_class_node, &parsed.source)
                )
            })?
    };

    let inner_mods = collect_java_modifiers(inner_class_node);
    if inner_mods.iter().any(|(name, _, _)| name == "static") {
        bail!(
            "error.bad_input(code=static_inner_class_in_promote): inner class `{inner_name}` is \
             declared static — it has no outer-state captures and does not need promotion. \
             Use `extract_java_nested_classes` for a syntactic move."
        );
    }

    // Collect inner class's own fields. Used to shadow bare-name captures: a
    // bare `field` inside the inner that matches an inner field is NOT an
    // outer capture.
    let inner_field_set: HashSet<String> = {
        let mut set = HashSet::new();
        if let Some(inner_body) = inner_class_node.child_by_field_name("body") {
            let mut cursor = inner_body.walk();
            for child in inner_body.named_children(&mut cursor) {
                if child.kind() == "field_declaration" {
                    if let Some(name) = java_field_declaration_name(child, &parsed.source) {
                        set.insert(name);
                    }
                }
            }
        }
        set
    };
    let outer_fields = outer_class_field_map(&parsed);
    if outer_fields.is_empty() {
        bail!(
            "outer class has no fields; promoting `{inner_name}` adds no captures and \
             `extract_java_nested_classes` is the better fit"
        );
    }

    // Walk the inner class body for outer-field captures + refusal cases.
    let analysis = analyze_inner_class_outer_refs(
        &parsed,
        inner_class_node,
        outer_class_node,
        &outer_fields,
        &inner_field_set,
    );

    if !analysis.outer_field_writes.is_empty() {
        bail!(
            "error.bad_input(code=inner_class_writes_outer_field): inner class `{inner_name}` \
             writes to outer field(s) {fields:?}. Captured outer fields are promoted to \
             `final` constructor parameters and cannot be reassigned. Refactor the writes \
             before promoting (e.g. accept a `Consumer` callback for the mutation).",
            fields = analysis.outer_field_writes
        );
    }
    if !analysis.outer_method_calls.is_empty() {
        bail!(
            "error.bad_input(code=inner_class_calls_outer_method): inner class `{inner_name}` \
             calls outer-class method(s) {methods:?}. v1 of promote_java_inner_class does not \
             thread outer-method calls; refactor the calls (e.g. inline, or accept a Runnable \
             callback param the outer instance can pass `this::method` to) before promoting.",
            methods = analysis.outer_method_calls
        );
    }

    // Find inner-class constructors. v1 supports 0 or 1 ctor without an
    // explicit `this(...)` chain. Multiple ctors or `this(...)` chains
    // refuse — definite-assignment of captures across every ctor path is
    // non-trivial and not worth v1 complexity.
    let inner_body = inner_class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("inner class `{inner_name}` has no body"))?;
    let inner_ctors: Vec<Node<'_>> = {
        let mut out = Vec::new();
        let mut cursor = inner_body.walk();
        for child in inner_body.named_children(&mut cursor) {
            if child.kind() == "constructor_declaration" {
                out.push(child);
            }
        }
        out
    };
    if inner_ctors.len() > 1 {
        bail!(
            "error.bad_input(code=inner_class_multiple_ctors): inner class `{inner_name}` has \
             {n} constructors. v1 supports at most one constructor (definite-assignment of \
             captures across multiple ctors is non-trivial). Consolidate before promoting.",
            n = inner_ctors.len()
        );
    }
    if let Some(ctor) = inner_ctors.first() {
        if constructor_has_this_chain(*ctor, &parsed.source) {
            bail!(
                "error.bad_input(code=inner_class_this_chain_ctor): inner class `{inner_name}`'s \
                 constructor delegates to another constructor via `this(...)`. v1 does not \
                 follow `this(...)` chains. Inline the delegation before promoting."
            );
        }
    }

    // Scan source for inner-class references beyond `new InnerClass(args)`.
    let usage_scan = scan_source_for_inner_class_uses(&parsed, &inner_name);
    if !usage_scan.non_new_sites.is_empty() {
        bail!(
            "error.bad_input(code=inner_class_referenced_as_type): inner class `{inner_name}` \
             is referenced outside `new {inner_name}(...)` at {sites:?}. v1 only rewrites \
             instantiation sites; type-position uses (variable declarations, casts, method \
             references, `Outer.Inner` paths) need manual handling.",
            sites = usage_scan.non_new_sites
        );
    }

    // Resolve target package via the unified resolver.
    let source_package = extract_java_package(&parsed.source);
    let target_package =
        resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let cross_package = match (source_package.as_deref(), target_package.as_deref()) {
        (Some(s), Some(t)) => s != t,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    };
    let visibility_floor = if cross_package { "public" } else { "package" };

    // Build the promoted class text on the target.
    let promoted_text = render_promoted_class(
        &parsed.source,
        inner_class_node,
        &inner_name,
        visibility_floor,
        &analysis.captures,
        inner_ctors.first().copied(),
    )?;

    let prelude = java_default_target_prelude(p, &parsed.source, target_package.as_deref());
    let raw_target_content = format!("{prelude}{promoted_text}");
    let project_dir_for_imports = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| target_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let target_content =
        match heuristic_java_organize_imports_text(&project_dir_for_imports, &raw_target_content) {
            Ok(pruned) => pruned,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "promote_java_inner_class: organize_imports failed; keeping unpruned"
                );
                raw_target_content
            }
        };

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    if !original_target_bytes.is_empty() {
        bail!("promote_java_inner_class currently requires a missing or empty target file");
    }

    // Source edits: remove inner declaration, rewrite each `new Inner(args)`,
    // and add cross-package import for the new top-level class.
    let mut source_edits: Vec<TextEdit> = Vec::new();
    source_edits.push(TextEdit {
        byte_start: java_node_leading_trivia_start(inner_class_node, &parsed.source),
        byte_end: inner_class_node.end_byte(),
        replacement: String::new(),
    });
    let capture_arg_text = analysis
        .captures
        .iter()
        .map(|c| c.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    for new_site in &usage_scan.new_sites {
        // Replace the entire `args` portion of the existing call (between
        // the outer `(` and `)`). Preserve operator-provided args and
        // append the captures.
        let existing = parsed.source[new_site.args_open + 1..new_site.args_close].trim();
        let merged = if existing.is_empty() {
            capture_arg_text.clone()
        } else if capture_arg_text.is_empty() {
            existing.to_string()
        } else {
            format!("{existing}, {capture_arg_text}")
        };
        source_edits.push(TextEdit {
            byte_start: new_site.args_open + 1,
            byte_end: new_site.args_close,
            replacement: merged,
        });
    }
    if cross_package {
        if let Some(tgt_pkg) = target_package.as_deref() {
            let fqcn = if tgt_pkg.is_empty() {
                inner_name.clone()
            } else {
                format!("{tgt_pkg}.{inner_name}")
            };
            if let Some(import_edit) = java_source_import_edit(&parsed.source, &fqcn) {
                source_edits.push(import_edit);
            }
        }
    }
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "Promote inner class {inner_name} from {} to {}",
            source_path.display(),
            target_path.display()
        ),
        kind: "promote_java_inner_class".to_string(),
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
                original_sha256: sha256_hex(&original_target_bytes),
                edits: vec![TextEdit {
                    byte_start: 0,
                    byte_end: original_target_bytes.len(),
                    replacement: target_content,
                }],
                new_text: None,
            },
        ],
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: analysis
            .captures
            .iter()
            .map(|c| CapturedVariable {
                name: c.name.clone(),
                kind: "field".to_string(),
                source_type: c.type_name.clone(),
                source_visibility: c.source_visibility.clone(),
                source_mutable: c.source_mutable,
                source_static_final: false,
            })
            .collect(),
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
struct PromotedCapture {
    name: String,
    type_name: String,
    source_visibility: String,
    source_mutable: bool,
}

#[derive(Debug, Default)]
struct InnerClassRefAnalysis {
    captures: Vec<PromotedCapture>,
    outer_field_writes: Vec<String>,
    outer_method_calls: Vec<String>,
}

/// Walk an inner class body, classify every identifier/method-invocation
/// against the outer class's field and method declarations, and return:
/// - `captures`: outer fields the inner READS (post-shadow)
/// - `outer_field_writes`: outer fields the inner WRITES (refusal trigger)
/// - `outer_method_calls`: outer-class methods the inner calls (refusal)
///
/// `this.field` is never treated as an outer capture (anonymous-class
/// boundary is the only place that re-binds `this`; lambdas inherit
/// enclosing `this`). `OuterClass.this.field` is always an outer capture.
fn analyze_inner_class_outer_refs(
    parsed: &ParsedSource,
    inner_class_node: Node<'_>,
    outer_class_node: Node<'_>,
    outer_fields: &BTreeMap<String, JavaField>,
    inner_field_set: &HashSet<String>,
) -> InnerClassRefAnalysis {
    let outer_name = java_class_name(outer_class_node, &parsed.source);
    let outer_methods: HashSet<String> = {
        let mut set = HashSet::new();
        if let Some(body) = outer_class_node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                if child.kind() == "method_declaration" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                            set.insert(name.to_string());
                        }
                    }
                }
            }
        }
        set
    };
    let mut captures: BTreeMap<String, PromotedCapture> = BTreeMap::new();
    let mut writes: BTreeSet<String> = BTreeSet::new();
    let mut methods: BTreeSet<String> = BTreeSet::new();

    let mut stack: Vec<(Node<'_>, bool)> = vec![(inner_class_node, false)];
    while let Some((node, inside_anonymous)) = stack.pop() {
        // Only `object_creation_expression` with an anonymous-class body
        // rebinds `this`. Plain `class_body` (the inner's own body, or any
        // nested class declaration's body) is NOT a new-`this` boundary.
        // Lambdas inherit enclosing `this`.
        let next_inside_anon = inside_anonymous
            || (node.kind() == "object_creation_expression" && {
                let mut cur = node.walk();
                node.named_children(&mut cur)
                    .any(|c| c.kind() == "class_body")
            });
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push((child, next_inside_anon));
        }

        // method_invocation: check if it's an outer-class method call.
        if node.kind() == "method_invocation" {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok());
            let Some(name) = name else { continue };
            if !outer_methods.contains(name) {
                continue;
            }
            // Distinguish receiver shape:
            // - no `object` (bare call) → outer method
            // - `object` is `this` → inner first; only outer if anon
            //   boundary makes it explicit. v1: treat as outer (rare).
            // - `object` is `<Outer>.this` → outer
            // - `object` is anything else → unrelated
            let object = node.child_by_field_name("object");
            let is_outer_call = match object {
                None => !inside_anonymous,
                Some(o) if o.kind() == "this" || o.kind() == "this_expression" => false,
                Some(o) => is_outer_qualified_this(o, &parsed.source, &outer_name),
            };
            if is_outer_call {
                methods.insert(name.to_string());
            }
            continue;
        }

        // assignment_expression: check if LHS resolves to an outer field.
        if node.kind() == "assignment_expression" {
            if let Some(left) = node.child_by_field_name("left") {
                if let Some(captured_name) =
                    classify_outer_field_access(left, parsed, outer_fields, inner_field_set, &outer_name, inside_anonymous)
                {
                    writes.insert(captured_name);
                }
            }
            continue;
        }
        if node.kind() == "update_expression" {
            // `field++`, `--field`, etc. — treat as write
            let mut ucur = node.walk();
            for c in node.named_children(&mut ucur) {
                if let Some(captured) =
                    classify_outer_field_access(c, parsed, outer_fields, inner_field_set, &outer_name, inside_anonymous)
                {
                    writes.insert(captured);
                }
            }
            continue;
        }

        // Bare identifier read or `Outer.this.field`. Classify as outer
        // capture if it resolves to an outer field.
        if let Some(captured_name) =
            classify_outer_field_access(node, parsed, outer_fields, inner_field_set, &outer_name, inside_anonymous)
        {
            if !writes.contains(&captured_name) {
                if !captures.contains_key(&captured_name) {
                    if let Some(field) = outer_fields.get(&captured_name) {
                        captures.insert(
                            captured_name.clone(),
                            PromotedCapture {
                                name: captured_name,
                                type_name: field.type_name.clone(),
                                source_visibility: "package".to_string(),
                                source_mutable: !field.is_final,
                            },
                        );
                    }
                }
            }
        }
    }
    let mut analysis = InnerClassRefAnalysis::default();
    analysis.captures = captures.into_values().collect();
    analysis.outer_field_writes = writes.into_iter().collect();
    analysis.outer_method_calls = methods.into_iter().collect();
    analysis
}

/// If `node` is an outer-field access — bare `identifier` matching an outer
/// field name (post inner-field shadow check) OR `OuterClass.this.field` —
/// return the field name. Otherwise return `None`.
fn classify_outer_field_access(
    node: Node<'_>,
    parsed: &ParsedSource,
    outer_fields: &BTreeMap<String, JavaField>,
    inner_field_set: &HashSet<String>,
    outer_name: &str,
    _inside_anonymous: bool,
) -> Option<String> {
    if node.kind() == "identifier" {
        let text = node.utf8_text(parsed.source.as_bytes()).ok()?;
        if !outer_fields.contains_key(text) {
            return None;
        }
        if inner_field_set.contains(text) {
            return None;
        }
        // Reject identifiers that are *names* of declarations or method
        // invocations (their identity is consumed by the parent node).
        if let Some(parent) = node.parent() {
            match parent.kind() {
                "variable_declarator"
                | "formal_parameter"
                | "spread_parameter"
                | "method_declaration"
                | "constructor_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "field_declaration"
                | "type_parameter"
                | "marker_annotation"
                | "annotation"
                | "enum_constant"
                | "labeled_statement" => return None,
                "method_invocation" => {
                    if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) {
                        return None;
                    }
                }
                "scoped_identifier" | "scoped_type_identifier" | "type_identifier"
                | "generic_type" => return None,
                "field_access" => {
                    // `something.field` — `field` part is consumed by the
                    // field_access classifier below; skip.
                    if parent.child_by_field_name("field").map(|c| c.id()) == Some(node.id()) {
                        return None;
                    }
                }
                _ => {}
            }
        }
        if is_shadowed(node, text, &parsed.source) {
            return None;
        }
        return Some(text.to_string());
    }
    if node.kind() == "field_access" {
        // `OuterClass.this.field` parses as field_access with object =
        // another field_access whose object is OuterClass-identifier and
        // field is `this` (or a `this_expression` form depending on
        // tree-sitter version). Be liberal: check for the `<Outer>.this`
        // pattern in the object subtree.
        let object = node.child_by_field_name("object")?;
        let field = node.child_by_field_name("field")?;
        let field_name = field.utf8_text(parsed.source.as_bytes()).ok()?;
        if !outer_fields.contains_key(field_name) {
            return None;
        }
        if is_outer_qualified_this(object, &parsed.source, outer_name) {
            return Some(field_name.to_string());
        }
    }
    None
}

/// Return true when `node` is a `<OuterClass>.this` expression (the
/// qualified-this form Java uses to disambiguate enclosing instances).
fn is_outer_qualified_this(node: Node<'_>, source: &str, outer_name: &str) -> bool {
    // Pattern 1: `field_access` with object = identifier(OuterClass), field = `this`.
    if node.kind() == "field_access" {
        if let (Some(obj), Some(fld)) = (
            node.child_by_field_name("object"),
            node.child_by_field_name("field"),
        ) {
            if obj.kind() == "identifier" && fld.kind() == "this" {
                if let Ok(text) = obj.utf8_text(source.as_bytes()) {
                    return text == outer_name;
                }
            }
        }
    }
    false
}

/// Returns true when the constructor's body starts with a `this(...)`
/// invocation (delegation to another ctor of the same class).
fn constructor_has_this_chain(ctor: Node<'_>, source: &str) -> bool {
    let Some(body) = ctor.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    let first_stmt = body.named_children(&mut cursor).next();
    let Some(stmt) = first_stmt else { return false };
    if stmt.kind() != "expression_statement" {
        return false;
    }
    let mut scur = stmt.walk();
    let inner = stmt.named_children(&mut scur).next();
    let Some(inner) = inner else { return false };
    if inner.kind() != "explicit_constructor_invocation" {
        return false;
    }
    // tree-sitter-java represents `this(...)` vs `super(...)` via an
    // unnamed `this` / `super` keyword child. Look at the source text.
    let text = match inner.utf8_text(source.as_bytes()) {
        Ok(t) => t.trim_start(),
        Err(_) => return false,
    };
    text.starts_with("this(") || text.starts_with("this (")
}

#[derive(Debug)]
struct InnerNewSite {
    args_open: usize,
    args_close: usize,
}

#[derive(Debug, Default)]
struct InnerClassUsageScan {
    new_sites: Vec<InnerNewSite>,
    non_new_sites: Vec<(usize, usize)>,
}

/// Scan the source AST for references to `inner_name` outside the inner
/// class's own declaration. Returns `new_sites` (positions of the args
/// list for each `new <Inner>(...)` instantiation) and `non_new_sites`
/// (every other type-position reference, which v1 refuses to handle).
fn scan_source_for_inner_class_uses(
    parsed: &ParsedSource,
    inner_name: &str,
) -> InnerClassUsageScan {
    let mut scan = InnerClassUsageScan::default();
    // Find the inner class node's range so we can skip it while walking.
    let inner_range = find_node(parsed.tree.root_node(), |node| {
        matches!(
            node.kind(),
            "class_declaration" | "record_declaration"
        ) && node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
            == Some(inner_name)
    })
    .map(|n| (n.start_byte(), n.end_byte()));
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if let Some((s, e)) = inner_range {
            if node.start_byte() >= s && node.end_byte() <= e {
                continue;
            }
        }
        if node.kind() == "object_creation_expression" {
            let type_node = node.child_by_field_name("type");
            let matches_inner = type_node
                .and_then(|t| t.utf8_text(parsed.source.as_bytes()).ok())
                .map(|s| s.trim() == inner_name)
                .unwrap_or(false);
            if matches_inner {
                if let Some(args) = node.child_by_field_name("arguments") {
                    scan.new_sites.push(InnerNewSite {
                        args_open: args.start_byte(),
                        args_close: args.end_byte() - 1,
                    });
                }
                continue;
            }
        }
        if matches!(node.kind(), "type_identifier" | "scoped_type_identifier") {
            if let Ok(text) = node.utf8_text(parsed.source.as_bytes()) {
                if text.trim() == inner_name {
                    // Skip the `type` slot of an enclosing
                    // `object_creation_expression` — that's an
                    // instantiation we've already counted as a new_site.
                    let is_new_type_slot = node
                        .parent()
                        .and_then(|p| {
                            if p.kind() == "object_creation_expression" {
                                p.child_by_field_name("type").map(|t| t.id())
                            } else {
                                None
                            }
                        })
                        == Some(node.id());
                    if !is_new_type_slot {
                        scan.non_new_sites.push((node.start_byte(), node.end_byte()));
                    }
                }
            }
        }
        if node.kind() == "method_reference" {
            let mut mcur = node.walk();
            if let Some(qualifier) = node.named_children(&mut mcur).next() {
                if let Ok(text) = qualifier.utf8_text(parsed.source.as_bytes()) {
                    if text.trim() == inner_name {
                        scan.non_new_sites.push((qualifier.start_byte(), qualifier.end_byte()));
                    }
                }
            }
        }
    }
    scan
}

/// Render the promoted top-level class text — class declaration with the
/// resolved visibility, captured-field declarations, a synthesized or
/// augmented constructor, and the inner class's original body content
/// (minus the inner constructor we may have rewritten).
fn render_promoted_class(
    source: &str,
    inner_class_node: Node<'_>,
    inner_name: &str,
    visibility_floor: &str,
    captures: &[PromotedCapture],
    inner_ctor: Option<Node<'_>>,
) -> Result<String> {
    // Reconstruct the class declaration line with the resolved visibility.
    // tree-sitter-java exposes `extends` / `implements` clauses as children
    // of the class_declaration; reuse the original text between the class
    // name and the opening `{`.
    let body = inner_class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("inner class `{inner_name}` has no body"))?;
    let header_end = body.start_byte();
    // Find the class keyword position (start of `class <Name>`).
    let header_text = source[inner_class_node.start_byte()..header_end].trim_end();
    // Strip leading modifiers and re-emit with the floor visibility.
    let class_kw = header_text
        .find("class ")
        .or_else(|| header_text.find("record "))
        .ok_or_else(|| anyhow!("could not locate `class` / `record` keyword in inner header"))?;
    let after_class_kw = &header_text[class_kw..];
    let visibility_prefix = if visibility_floor == "package" {
        String::new()
    } else {
        format!("{visibility_floor} ")
    };
    let class_decl_text = format!("{visibility_prefix}{after_class_kw}");

    // Captured-field declarations on the promoted class.
    let captured_fields = captures
        .iter()
        .map(|c| format!("    private final {} {};", c.type_name, c.name))
        .collect::<Vec<_>>()
        .join("\n");

    // Constructor: synthesize when the inner has no ctor; otherwise extend
    // the existing ctor's signature and prepend capture-assignments after
    // any leading `super(...)` chain.
    let (ctor_text, ctor_remove_range) = match inner_ctor {
        None => (
            synthesize_promoted_ctor(inner_name, captures, visibility_floor),
            None,
        ),
        Some(ctor) => {
            let extended = extend_existing_ctor_with_captures(
                source,
                ctor,
                inner_name,
                captures,
                visibility_floor,
            )?;
            (
                extended,
                Some((
                    java_node_leading_trivia_start(ctor, source),
                    ctor.end_byte(),
                )),
            )
        }
    };

    // Inner body content, minus the ctor we just rewrote.
    let body_open = body.start_byte() + 1; // past the `{`
    let body_close = body.end_byte() - 1; // before the `}`
    let mut inner_body_text = String::new();
    if let Some((rm_start, rm_end)) = ctor_remove_range {
        inner_body_text.push_str(&source[body_open..rm_start]);
        inner_body_text.push_str(&source[rm_end..body_close]);
    } else {
        inner_body_text.push_str(&source[body_open..body_close]);
    }
    let inner_body_trimmed = inner_body_text.trim_matches('\n').to_string();

    let mut body_parts: Vec<String> = Vec::new();
    if !captured_fields.trim().is_empty() {
        body_parts.push(captured_fields);
    }
    if !ctor_text.trim().is_empty() {
        body_parts.push(ctor_text);
    }
    if !inner_body_trimmed.trim().is_empty() {
        body_parts.push(inner_body_trimmed);
    }
    let assembled_body = body_parts.join("\n\n");
    Ok(format!("{class_decl_text} {{\n{assembled_body}\n}}\n"))
}

fn synthesize_promoted_ctor(
    inner_name: &str,
    captures: &[PromotedCapture],
    visibility_floor: &str,
) -> String {
    if captures.is_empty() {
        return String::new();
    }
    let vis = if visibility_floor == "package" {
        String::new()
    } else {
        format!("{visibility_floor} ")
    };
    let params = captures
        .iter()
        .map(|c| format!("{} {}", c.type_name, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let assigns = captures
        .iter()
        .map(|c| format!("        this.{n} = {n};", n = c.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!("    {vis}{inner_name}({params}) {{\n{assigns}\n    }}")
}

/// Extend an existing inner constructor: append captured params to its
/// signature, then prepend `this.x = x;` assignments to the body AFTER any
/// `super(...)` first-statement. Returns the new constructor text suitable
/// for placement on the promoted class.
fn extend_existing_ctor_with_captures(
    source: &str,
    ctor: Node<'_>,
    inner_name: &str,
    captures: &[PromotedCapture],
    visibility_floor: &str,
) -> Result<String> {
    // Existing ctor text (verbatim).
    let raw = source[ctor.start_byte()..ctor.end_byte()].to_string();
    if captures.is_empty() {
        return Ok(format!("    {raw}"));
    }
    // 1. Rewrite the visibility modifier to the floor (drop private/protected
    //    that only made sense for an inner ctor).
    let mods = collect_java_modifiers(ctor);
    let new_visibility = if visibility_floor == "package" {
        None
    } else {
        Some(visibility_floor)
    };
    let vis_edit = build_visibility_rewrite_edit(ctor, &mods, new_visibility, source);
    // Translate visibility edit into local (ctor-relative) coordinates.
    let local_vis_start = vis_edit.byte_start - ctor.start_byte();
    let local_vis_end = vis_edit.byte_end - ctor.start_byte();
    let mut rewritten = raw.clone();
    rewritten.replace_range(local_vis_start..local_vis_end, &vis_edit.replacement);

    // 2. Append captured params to the ctor signature.
    let params_node = ctor
        .child_by_field_name("parameters")
        .ok_or_else(|| anyhow!("ctor has no parameters node"))?;
    let params_open_local = params_node.start_byte() - ctor.start_byte();
    let params_close_local = params_node.end_byte() - ctor.start_byte();
    let existing_params = &rewritten[params_open_local + 1..params_close_local - 1];
    let extra_params = captures
        .iter()
        .map(|c| format!("{} {}", c.type_name, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let merged_params = if existing_params.trim().is_empty() {
        extra_params
    } else {
        format!("{}, {extra_params}", existing_params.trim())
    };
    let merged_with_parens = format!("({merged_params})");
    rewritten.replace_range(params_open_local..params_close_local, &merged_with_parens);

    // 3. Prepend `this.x = x;` after super(...) if present.
    // Re-parse the rewritten ctor to find the body + first-statement boundary.
    let reparse_text = format!("class __Tmp {{ {rewritten} }}");
    let tree = parse_source("java", &reparse_text)?;
    let class_node = tree
        .root_node()
        .named_child(0)
        .ok_or_else(|| anyhow!("reparse: no class node"))?;
    let class_body = class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("reparse: class body missing"))?;
    let mut bcur = class_body.walk();
    let new_ctor = class_body
        .named_children(&mut bcur)
        .find(|c| c.kind() == "constructor_declaration")
        .ok_or_else(|| anyhow!("reparse: ctor missing"))?;
    let new_body = new_ctor
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("reparse: ctor body missing"))?;
    // Determine insertion point: after super(...) first-statement if any,
    // otherwise at body open.
    let body_open = new_body.start_byte() + 1; // past `{`
    let mut insert_at_local = body_open - "class __Tmp { ".len();
    let mut scur = new_body.walk();
    if let Some(first_stmt) = new_body.named_children(&mut scur).next() {
        if first_stmt.kind() == "expression_statement" {
            let mut ecur = first_stmt.walk();
            if let Some(inner_expr) = first_stmt.named_children(&mut ecur).next() {
                if inner_expr.kind() == "explicit_constructor_invocation" {
                    if let Ok(t) = inner_expr.utf8_text(reparse_text.as_bytes()) {
                        if t.trim_start().starts_with("super") {
                            insert_at_local =
                                first_stmt.end_byte() - "class __Tmp { ".len();
                        }
                    }
                }
            }
        }
    }
    // Assignment lines.
    let assigns = captures
        .iter()
        .map(|c| format!("\n        this.{n} = {n};", n = c.name))
        .collect::<String>();
    rewritten.insert_str(insert_at_local, &assigns);

    let _ = inner_name; // (name preserved by raw ctor text)
    Ok(format!("    {rewritten}"))
}

/// Return the byte offset of the start of `node`'s leading whitespace /
/// comment block. Used to delete a member declaration along with its
/// preceding blank line / Javadoc.
fn java_node_leading_trivia_start(node: Node<'_>, source: &str) -> usize {
    let mut start = node.start_byte();
    while start > 0 {
        let prev = source.as_bytes()[start - 1];
        if prev == b' ' || prev == b'\t' {
            start -= 1;
        } else {
            break;
        }
    }
    start
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
        new_text: None,
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract {} nested classes to {}",
            selected.len(),
            target_path.display()
        ),
        kind: "extract_java_nested_classes".to_string(),
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
            target_edit,
        ],
        validations: vec![],
        items: Vec::new(),
        leftovers: Vec::new(),
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
    let replacement = format!("\n{}", declarations.trim_end());
    let plan = RefactorPlan {
        title: format!(
            "Add {} Java field(s) to {}",
            fields.len(),
            source_path.display()
        ),
        kind: "add_java_fields".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
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
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement: format!("\n\n{}", constructor.trim_end()),
            }],
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
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
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(source_parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_parsed.source.as_bytes()),
                edits: vec![TextEdit {
                    byte_start: insert_at,
                    byte_end: insert_at,
                    replacement: format!("\n{}", moved_text),
                }],
                new_text: None,
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
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
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
        let resolved_pkg =
            resolve_java_target_package(p, &source_parsed.source, &source_path, &target_path)?;
        let prelude =
            java_default_target_prelude(p, &source_parsed.source, resolved_pkg.as_deref());
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
            new_text: None,
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
        new_text: None,
    });

    let plan = RefactorPlan {
        title: format!(
            "Move {} Java constant(s) from {} to {}",
            selected.len(),
            source_path.display(),
            target_path.display()
        ),
        kind: "move_java_constant".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
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
                if matches!(parent.kind(), "parenthesized_expression" | "cast_expression") {
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
    let mut rhs_sub_edits: Vec<Vec<TextEdit>> = (0..lhs_write_sites.len())
        .map(|_| Vec::new())
        .collect();

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
        let absorbed = lhs_write_sites
            .iter()
            .position(|site| {
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
            if matches!(parent.kind(), "parenthesized_expression" | "cast_expression") {
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
        let mut rhs_text =
            parsed.source[site.rhs.start_byte()..site.rhs.end_byte()].to_string();
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
        leftovers: Vec::new(),
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
fn classify_callback_method(
    parsed: &ParsedSource,
    method_node: Node<'_>,
) -> Result<CallbackSpec> {
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
        (0, true) => (
            "Runnable".to_string(),
            "run".to_string(),
            None,
        ),
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
        let args_inner = args_text.trim().trim_start_matches('(').trim_end_matches(')').trim();
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
                if matches!(parent.kind(), "parenthesized_expression" | "cast_expression") {
                    target = parent;
                    continue;
                }
                break;
            }
            if let Some(parent) = target.parent() {
                match parent.kind() {
                    "assignment_expression" => {
                        if parent.child_by_field_name("left").map(|c| c.id())
                            == Some(target.id())
                        {
                            return true;
                        }
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
        leftovers: Vec::new(),
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
        leftovers: Vec::new(),
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
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
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
                    && m.item.name.as_deref() != Some("<init>")
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
            let ident = chunk.split_whitespace().last().unwrap_or("").trim();
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
let ident = chunk.split_whitespace().last().unwrap_or("").trim();
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
        new_text: None,
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
        new_text: None,
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
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
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
        leftovers: Vec::new(),
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

use lsp_types::{
    request::CodeActionRequest, CodeActionContext, CodeActionKind, CodeActionParams, Position,
    Range, TextDocumentIdentifier,
};

use crate::lsp::{LspError, LspSessionManager};
use crate::projects::Language;
use cross_file::{MovedStaticItem, compute_cross_file_static_caller_edits};
pub(crate) use lombokify::plan_lombokify_java_class;

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
                SemanticStatus::SyntaxOnly,
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
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
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

#[cfg(test)]
mod tests;
mod lombokify;

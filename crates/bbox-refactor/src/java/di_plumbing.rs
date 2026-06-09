//! Shared DI-plumbing substrate for caller-side Java refactor
//! primitives.
//!
//! Five caller-side primitives need to thread a new typed receiver
//! field through a class:
//!
//! - `migrate_java_method_receiver` — rewrite holder.foo() →
//!   newHolder.foo()
//! - `java_split_provider` — split Provider<Big> by getter into typed
//!   providers
//! - `replace_java_static_reference` (under three atoms) — rewrite
//!   `HolderClass.X` / `*Util.method()` / `UI.getCurrent()` →
//!   `provider.get().X` / `provider.get().method()` / `provider.get()`
//!
//! All five v1s refused when the new receiver field didn't already
//! exist on the enclosing class. This module's helpers let them
//! generate the `@Inject Provider<T>` field + matching `import`
//! addition as part of the plan, eliminating the refusal class.
//!
//! ## Capability surface
//!
//! - `InjectNamespace::detect(source)` — return `Jakarta`, `Javax`, or
//!   `Unspecified` by reading existing `import jakarta.inject.*` /
//!   `import javax.inject.*` directives at the top of the file.
//! - `FieldKind` — Direct (`@Inject Foo foo;`) vs Provider
//!   (`@Inject Provider<Foo> fooProvider;`).
//! - `find_existing_field(class_node, source, type_name)` — find an
//!   already-declared `@Inject`-style field whose type matches
//!   `type_name` (direct OR provider). Returns the field name and
//!   kind, plus the canonical receiver expression to use at call sites
//!   (`<name>` for Direct, `<name>.get()` for Provider).
//! - `ensure_inject_field(class_node, source, type_name, prefer_provider)`
//!   — find an existing field OR generate a `TextEdit` that adds the
//!   needed `@Inject` declaration plus the matching `import` line if
//!   not already present.

use std::collections::HashSet;
use tree_sitter::Node;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectNamespace {
    Jakarta,
    Javax,
    Unspecified,
}

impl InjectNamespace {
    /// Detect by examining import declarations in `source`.
    pub fn detect(source: &str) -> InjectNamespace {
        if source.contains("import jakarta.inject.") {
            InjectNamespace::Jakarta
        } else if source.contains("import javax.inject.") {
            InjectNamespace::Javax
        } else {
            InjectNamespace::Unspecified
        }
    }

    pub fn inject_fqcn(self) -> &'static str {
        match self {
            InjectNamespace::Jakarta => "jakarta.inject.Inject",
            InjectNamespace::Javax => "javax.inject.Inject",
            InjectNamespace::Unspecified => "jakarta.inject.Inject",
        }
    }

    pub fn provider_fqcn(self) -> &'static str {
        match self {
            InjectNamespace::Jakarta => "jakarta.inject.Provider",
            InjectNamespace::Javax => "javax.inject.Provider",
            InjectNamespace::Unspecified => "jakarta.inject.Provider",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// `@Inject Foo foo;`
    Direct,
    /// `@Inject Provider<Foo> fooProvider;`
    Provider,
}

#[derive(Debug, Clone)]
pub struct ExistingField {
    pub name: String,
    pub kind: FieldKind,
}

impl ExistingField {
    /// Receiver expression to splice at a call site (`foo` for Direct,
    /// `fooProvider.get()` for Provider).
    pub fn receiver_expr(&self) -> String {
        match self.kind {
            FieldKind::Direct => self.name.clone(),
            FieldKind::Provider => format!("{}.get()", self.name),
        }
    }
}

/// Find an `@Inject`-annotated field on `class_node` whose type matches
/// `type_name` (either direct `Foo` or `Provider<Foo>`). Returns the
/// canonical field info, or None if no match.
pub fn find_existing_field(
    class_node: Node<'_>,
    source: &str,
    type_name: &str,
) -> Option<ExistingField> {
    let bytes = source.as_bytes();
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let has_inject = field_has_annotation(member, source, "Inject");
        if !has_inject {
            continue;
        }
        let Some(type_node) = member.child_by_field_name("type") else {
            continue;
        };
        let field_type = type_node.utf8_text(bytes).unwrap_or("").trim();
        let kind = if field_type == type_name {
            FieldKind::Direct
        } else if matches_provider_of(field_type, type_name) {
            FieldKind::Provider
        } else {
            continue;
        };
        // Use the first declarator's name (multi-declarator @Inject is
        // pathological; we ignore the second+).
        let mut mc = member.walk();
        for decl in member.named_children(&mut mc) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = decl.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(bytes) {
                    return Some(ExistingField {
                        name: name.to_string(),
                        kind,
                    });
                }
            }
        }
    }
    None
}

fn matches_provider_of(field_type: &str, type_name: &str) -> bool {
    let trimmed = field_type.trim();
    if let Some(inner) = trimmed
        .strip_prefix("Provider<")
        .or_else(|| trimmed.strip_prefix("jakarta.inject.Provider<"))
        .or_else(|| trimmed.strip_prefix("javax.inject.Provider<"))
    {
        if let Some(inner) = inner.strip_suffix('>') {
            return inner.trim() == type_name;
        }
    }
    false
}

fn field_has_annotation(member: Node<'_>, source: &str, annotation: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = member.walk();
    for child in member.children(&mut cursor) {
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

/// Result of `ensure_inject_field`: the field info to use at call
/// sites, plus optional FileEdits that must be applied to the source
/// file when the field didn't already exist.
#[derive(Debug, Clone)]
pub struct EnsuredField {
    pub field: ExistingField,
    /// Edits to splice into the source file (in any order — they're
    /// non-overlapping). Empty when the field already existed.
    pub edits: Vec<TextEdit>,
}

/// Find an existing `@Inject` field matching `type_name`, or generate
/// a `TextEdit` that adds one to the class body (plus the matching
/// `import` line if not already present).
///
/// `prefer_provider=true` synthesizes `@Inject Provider<T> tProvider;`.
/// `prefer_provider=false` synthesizes `@Inject T t;`.
pub fn ensure_inject_field(
    class_node: Node<'_>,
    source: &str,
    type_name: &str,
    prefer_provider: bool,
) -> Result<EnsuredField> {
    if let Some(existing) = find_existing_field(class_node, source, type_name) {
        return Ok(EnsuredField {
            field: existing,
            edits: Vec::new(),
        });
    }
    let ns = InjectNamespace::detect(source);
    let field_name = derive_field_name(type_name, prefer_provider);
    let (decl_text, decl_kind) = if prefer_provider {
        (
            format!("    @Inject\n    private Provider<{type_name}> {field_name};\n"),
            FieldKind::Provider,
        )
    } else {
        (
            format!("    @Inject\n    private {type_name} {field_name};\n"),
            FieldKind::Direct,
        )
    };

    let insert_byte = class_field_insert_byte(class_node)
        .ok_or_else(|| anyhow!("could not find a place to insert the @Inject field"))?;

    let mut edits = vec![TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: decl_text,
    }];

    // Ensure `import <ns>.Inject;` and (when Provider) `import <ns>.Provider;`.
    let import_needs = vec![
        Some(ns.inject_fqcn().to_string()),
        if prefer_provider {
            Some(ns.provider_fqcn().to_string())
        } else {
            None
        },
    ];
    for opt in import_needs.into_iter().flatten() {
        if let Some(edit) = ensure_import_edit(source, &opt) {
            edits.push(edit);
        }
    }
    edits.sort_by_key(|e| e.byte_start);

    Ok(EnsuredField {
        field: ExistingField {
            name: field_name,
            kind: decl_kind,
        },
        edits,
    })
}

/// Derive a Java field name from a type name. `Foo` → `foo`; for
/// `prefer_provider=true`, suffix `Provider`: `Foo` → `fooProvider`.
fn derive_field_name(type_name: &str, prefer_provider: bool) -> String {
    let bare = type_name
        .split('<')
        .next()
        .unwrap_or(type_name)
        .rsplit('.')
        .next()
        .unwrap_or(type_name);
    let mut chars = bare.chars();
    let lower = match chars.next() {
        Some(c) => c.to_lowercase().to_string(),
        None => String::new(),
    };
    let rest: String = chars.collect();
    let base = format!("{lower}{rest}");
    if prefer_provider {
        format!("{base}Provider")
    } else {
        base
    }
}

/// Walk class body for an insertion point. Prefers right after the last
/// field_declaration; falls back to right after the opening brace.
fn class_field_insert_byte(class_node: Node<'_>) -> Option<usize> {
    let body = class_node.child_by_field_name("body")?;
    let mut last_field_end: Option<usize> = None;
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() == "field_declaration" {
            last_field_end = Some(member.end_byte());
        }
    }
    if let Some(end) = last_field_end {
        return Some(end + 1); // after the newline
    }
    // No fields — insert just after the opening brace.
    Some(body.start_byte() + 1)
}

/// Produce a `TextEdit` that adds `import <fqcn>;\n` to the file's
/// import block, or `None` if the import is already present.
fn ensure_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let needle = format!("import {fqcn};");
    if source.contains(&needle) {
        return None;
    }
    // Find insertion point: after the last existing import, or after the
    // package declaration if no imports yet, or at byte 0.
    let imports = extract_existing_imports(source);
    // clippy suggests collapsing the inner Option into unwrap_or, but the three-way
    // chain (imports → package decl → 0) reads more clearly as if/else if/else.
    #[allow(clippy::manual_unwrap_or, clippy::manual_unwrap_or_default)]
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
        start = line_end + 1; // +1 for the newline
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

// Used by callers that need to merge a set of `TextEdit`s without
// duplicating concurrent insertion points.
pub fn dedupe_insertion_edits(edits: Vec<TextEdit>) -> Vec<TextEdit> {
    let mut seen: HashSet<(usize, usize, String)> = HashSet::new();
    let mut out = Vec::new();
    for e in edits {
        let key = (e.byte_start, e.byte_end, e.replacement.clone());
        if seen.insert(key) {
            out.push(e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::code::parser_for_language;

    fn parse(src: &str) -> (tree_sitter::Tree, Node<'_>) {
        let mut parser = parser_for_language("java").unwrap();
        let tree = parser.parse(src, None).unwrap();
        // Find first class_declaration.
        let mut stack = vec![tree.root_node()];
        while let Some(n) = stack.pop() {
            if matches!(
                n.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "record_declaration"
                    | "enum_declaration"
            ) {
                // SAFETY: returning a node tied to the tree we own; caller must keep tree alive.
                let class_node = unsafe { std::mem::transmute::<Node<'_>, Node<'_>>(n) };
                return (tree, class_node);
            }
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                stack.push(ch);
            }
        }
        panic!("no class found");
    }

    #[test]
    fn detect_jakarta_namespace() {
        let src = "package p;\nimport jakarta.inject.Inject;\nclass T {}";
        assert_eq!(InjectNamespace::detect(src), InjectNamespace::Jakarta);
    }

    #[test]
    fn detect_javax_namespace() {
        let src = "package p;\nimport javax.inject.Inject;\nclass T {}";
        assert_eq!(InjectNamespace::detect(src), InjectNamespace::Javax);
    }

    #[test]
    fn detect_unspecified_namespace_defaults_jakarta() {
        let src = "package p;\nclass T {}";
        let ns = InjectNamespace::detect(src);
        assert_eq!(ns, InjectNamespace::Unspecified);
        assert_eq!(ns.inject_fqcn(), "jakarta.inject.Inject");
    }

    #[test]
    fn find_existing_direct_inject_field() {
        let src = "package p;\nimport jakarta.inject.Inject;\nclass T { @Inject private Foo foo; }";
        let (_tree, class_node) = parse(src);
        let found = find_existing_field(class_node, src, "Foo").unwrap();
        assert_eq!(found.name, "foo");
        assert_eq!(found.kind, FieldKind::Direct);
        assert_eq!(found.receiver_expr(), "foo");
    }

    #[test]
    fn find_existing_provider_inject_field() {
        let src = "package p;\nimport jakarta.inject.Inject;\nimport jakarta.inject.Provider;\nclass T { @Inject private Provider<Foo> fooProvider; }";
        let (_tree, class_node) = parse(src);
        let found = find_existing_field(class_node, src, "Foo").unwrap();
        assert_eq!(found.name, "fooProvider");
        assert_eq!(found.kind, FieldKind::Provider);
        assert_eq!(found.receiver_expr(), "fooProvider.get()");
    }

    #[test]
    fn find_existing_returns_none_when_no_match() {
        let src = "package p;\nimport jakarta.inject.Inject;\nclass T { @Inject private Bar bar; }";
        let (_tree, class_node) = parse(src);
        assert!(find_existing_field(class_node, src, "Foo").is_none());
    }

    #[test]
    fn ensure_field_returns_existing_when_present() {
        let src = "package p;\nimport jakarta.inject.Inject;\nclass T { @Inject private Foo foo; }";
        let (_tree, class_node) = parse(src);
        let ensured = ensure_inject_field(class_node, src, "Foo", false).unwrap();
        assert_eq!(ensured.field.name, "foo");
        assert!(
            ensured.edits.is_empty(),
            "no edits expected when field exists"
        );
    }

    #[test]
    fn ensure_field_generates_direct_inject_when_missing() {
        let src = "package p;\nimport jakarta.inject.Inject;\nclass T {\n}";
        let (_tree, class_node) = parse(src);
        let ensured = ensure_inject_field(class_node, src, "Foo", false).unwrap();
        assert_eq!(ensured.field.name, "foo");
        assert_eq!(ensured.field.kind, FieldKind::Direct);
        // One edit: the field declaration. (Inject import already present.)
        let field_edit = ensured
            .edits
            .iter()
            .find(|e| e.replacement.contains("@Inject"))
            .unwrap();
        assert!(field_edit.replacement.contains("private Foo foo;"));
    }

    #[test]
    fn ensure_field_generates_provider_when_preferred() {
        let src = "package p;\nimport jakarta.inject.Inject;\nimport jakarta.inject.Provider;\nclass T {\n}";
        let (_tree, class_node) = parse(src);
        let ensured = ensure_inject_field(class_node, src, "Foo", true).unwrap();
        assert_eq!(ensured.field.name, "fooProvider");
        assert_eq!(ensured.field.kind, FieldKind::Provider);
        assert_eq!(ensured.field.receiver_expr(), "fooProvider.get()");
        let field_edit = ensured
            .edits
            .iter()
            .find(|e| e.replacement.contains("@Inject"))
            .unwrap();
        assert!(field_edit.replacement.contains("Provider<Foo> fooProvider"));
    }

    #[test]
    fn ensure_field_adds_imports_when_missing() {
        let src = "package p;\nclass T {\n}";
        let (_tree, class_node) = parse(src);
        let ensured = ensure_inject_field(class_node, src, "Foo", true).unwrap();
        let inject_import = ensured
            .edits
            .iter()
            .find(|e| e.replacement.contains("import jakarta.inject.Inject;"));
        let provider_import = ensured
            .edits
            .iter()
            .find(|e| e.replacement.contains("import jakarta.inject.Provider;"));
        assert!(inject_import.is_some(), "should add Inject import");
        assert!(provider_import.is_some(), "should add Provider import");
    }

    #[test]
    fn ensure_field_chooses_javax_when_already_in_use() {
        let src = "package p;\nimport javax.inject.Inject;\nclass T {\n}";
        let (_tree, class_node) = parse(src);
        let ensured = ensure_inject_field(class_node, src, "Foo", true).unwrap();
        let provider_import = ensured
            .edits
            .iter()
            .find(|e| e.replacement.contains("javax.inject.Provider"));
        assert!(
            provider_import.is_some(),
            "should pick javax namespace to match existing imports"
        );
    }

    #[test]
    fn derive_field_name_strips_generics_and_packages() {
        assert_eq!(derive_field_name("Foo", false), "foo");
        assert_eq!(derive_field_name("com.example.Foo", false), "foo");
        assert_eq!(derive_field_name("Foo<Bar>", false), "foo");
        assert_eq!(derive_field_name("Foo", true), "fooProvider");
    }
}

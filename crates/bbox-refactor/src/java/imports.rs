use super::*;

pub(super) fn extract_java_package(source: &str) -> Option<String> {
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

pub(super) fn extract_java_imports(source: &str) -> Vec<String> {
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
pub(super) fn derive_java_package_from_path(target_path: &Path) -> Option<String> {
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
pub(super) fn resolve_java_target_package(
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
pub(super) fn java_source_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let import_line = format!("import {fqcn};");
    if source.lines().any(|line| line.trim() == import_line) {
        return None;
    }
    // G7-FU: dedupe by simple name too. Two FQCNs sharing a simple name
    // (e.g. `com.google.inject.Inject` vs `javax.inject.Inject`) can't
    // both be imported — Java rejects duplicate simple-name imports.
    // Skip the new import when the source already has any single-type
    // import with the same simple name.
    //
    // Wildcard guard: skip only when an existing wildcard import covers
    // the SAME package as the new import (in which case the explicit
    // import is redundant and the source already binds the simple name
    // from that wildcard). Foreign wildcards (`import a.b.*;` while
    // adding `import x.y.Foo;`) don't supply the new simple name and
    // must not block the addition — the previous blanket skip was too
    // aggressive and silently dropped legitimate imports.
    let desired_simple = fqcn.rsplit('.').next().unwrap_or("");
    let desired_pkg = java_fqcn_package(fqcn);
    if !desired_simple.is_empty() {
        for line in source.lines() {
            if let Some(existing) = java_import_simple_name(line) {
                if existing == desired_simple {
                    return None;
                }
            }
            if let Some(wildcard_pkg) = java_import_wildcard_package(line) {
                if Some(wildcard_pkg.as_str()) == desired_pkg {
                    return None;
                }
            }
        }
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

/// Return the package prefix of a non-static wildcard import, e.g.
/// `import a.b.*;` → `Some("a.b")`. Returns `None` for non-wildcard,
/// static wildcard, or malformed import lines.
pub(super) fn java_import_wildcard_package(import_line: &str) -> Option<String> {
    let body = import_line
        .trim()
        .strip_prefix("import ")
        .and_then(|s| s.strip_suffix(';'))
        .map(str::trim)?;
    if body.starts_with("static ") {
        return None;
    }
    body.strip_suffix(".*").map(str::to_string)
}

/// Return the package prefix of a fully-qualified class name, e.g.
/// `a.b.Foo` → `Some("a.b")`. Returns `None` for unqualified names.
pub(super) fn java_fqcn_package(fqcn: &str) -> Option<&str> {
    fqcn.rsplit_once('.').map(|(pkg, _)| pkg)
}

pub(super) fn java_import_simple_name(import_line: &str) -> Option<String> {
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

pub(super) fn java_import_static_member_simple_name(import_line: &str) -> Option<String> {
    let body = import_line
        .trim()
        .strip_prefix("import static ")?
        .trim_end_matches(';')
        .trim();
    if body.ends_with(".*") {
        return None;
    }
    body.rsplit('.').next().map(str::to_string)
}

pub(super) fn java_builtin_type(name: &str) -> bool {
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
            | "FunctionalInterface"
            | "SafeVarargs"
    )
}

pub(super) fn collect_java_type_references(
    node: Node<'_>,
    source: &str,
    out: &mut HashSet<String>,
) {
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
    // G16: annotation references. `@Nullable`, `@Transactional`, custom
    // domain annotations referenced in moved bodies and method signatures
    // need their imports preserved on the extracted target. tree-sitter-
    // java parses:
    //   - `marker_annotation` for `@Foo` (no args)
    //   - `annotation` for `@Foo(args)` and `@Foo(key = "value")`
    //   - `annotated_type` for `@Nullable String foo` — the annotation
    //     itself parses as one of the above as a child.
    // The annotation's name is the first named child (`identifier`,
    // `scoped_identifier`, or `type_identifier`). For scoped names like
    // `@some.pkg.Annot` the simple name is the last segment — we add the
    // simple name to the type-reference set so the organize-imports pass
    // can route through the project type index just like type_identifier
    // references. JDK built-ins (`@Override`, `@Deprecated`,
    // `@SuppressWarnings`) are already in java_builtin_type and get
    // filtered out.
    if matches!(node.kind(), "marker_annotation" | "annotation") {
        let mut cursor = node.walk();
        if let Some(name_node) = node.named_children(&mut cursor).next() {
            let name_text = match name_node.kind() {
                "identifier" | "type_identifier" => name_node.utf8_text(source.as_bytes()).ok(),
                "scoped_identifier" => {
                    // scoped_identifier wraps `pkg.Name` — the simple name
                    // is its last named child.
                    let mut sc = name_node.walk();
                    name_node
                        .named_children(&mut sc)
                        .last()
                        .and_then(|c| c.utf8_text(source.as_bytes()).ok())
                }
                _ => None,
            };
            if let Some(text) = name_text {
                if text.chars().next().is_some_and(|c| c.is_uppercase()) && !java_builtin_type(text)
                {
                    out.insert(text.to_string());
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_type_references(child, source, out);
    }
}

pub(super) fn collect_java_static_member_references(
    node: Node<'_>,
    source: &str,
    out: &mut HashSet<String>,
) {
    if node.kind() == "import_declaration" {
        return;
    }
    if matches!(node.kind(), "identifier" | "field_identifier") {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            out.insert(text.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_static_member_references(child, source, out);
    }
}

pub(super) fn java_import_block_range(source: &str) -> (usize, usize, usize) {
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
pub(super) struct JavaTypeIndex {
    /// Simple-name → uniquely-resolvable FQCN for *top-level* types,
    /// or `None` when the simple name is ambiguous across packages.
    /// Mirrors the historical `project_java_type_index` shape.
    pub(super) top_level: BTreeMap<String, Option<String>>,
    /// Simple names of inner classes (members of class/interface/
    /// record/enum bodies) discovered in the project. Inner-class
    /// references must be left in qualified form (`Outer.Inner`)
    /// rather than imported as a bare simple name; gap 16 lives
    /// here.
    pub(super) inner_class_names: HashSet<String>,
}

pub(super) fn build_java_type_index(project_dir: &Path) -> Result<JavaTypeIndex> {
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
pub(super) fn collect_inner_class_simple_names(
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
        || matches!(
            kind,
            "class_body" | "enum_body" | "record_body" | "interface_body"
        );
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_inner_class_simple_names(child, source, next_inside, out);
    }
}

pub(super) fn heuristic_java_organize_imports(
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
/// wildcard imports verbatim, prunes regular imports whose simple name does not
/// appear in `type_identifier` references in the AST, prunes single-member
/// static imports whose member name is absent from the AST, adds project-local
/// imports for unresolved simple names, and skips inner-class simple names (gap
/// 16). On parse failure or when no rewrite is needed the input string is
/// returned unchanged.
pub(super) fn heuristic_java_organize_imports_text(
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
pub(super) fn compute_java_organize_imports_edit(
    project_dir: &Path,
    source: &str,
    tree: &Tree,
) -> Result<Option<(usize, usize, String)>> {
    let mut used_types = HashSet::new();
    collect_java_type_references(tree.root_node(), source, &mut used_types);
    let mut used_static_members = HashSet::new();
    collect_java_static_member_references(tree.root_node(), source, &mut used_static_members);
    let current_package = extract_java_package(source);
    let existing_imports = extract_java_imports(source);
    let mut imports = existing_imports
        .iter()
        .filter(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("import static ") {
                if trimmed.ends_with(".*;") {
                    return true;
                }
                return java_import_static_member_simple_name(trimmed)
                    .is_some_and(|simple| used_static_members.contains(simple.as_str()));
            }
            if trimmed.ends_with(".*;") {
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
    // listing them again is redundant. `import static …` lines that survived
    // the member-usage filter above are NEVER dropped here (they bring members,
    // not types — wildcards on types do not cover them) and explicit imports
    // from packages without a matching wildcard are left alone.
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

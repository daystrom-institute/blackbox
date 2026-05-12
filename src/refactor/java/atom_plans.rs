use super::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};

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

    // Gap 10: derive package + imports for the target, matching what
    // extract_java_class already does. Without this the emitted file
    // has no package decl and no imports, so every type reference
    // fails javac with `cannot find symbol`.
    let target_package = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let source_package = extract_java_package(&parsed.source);
    let cross_package = match (source_package.as_deref(), target_package.as_deref()) {
        (Some(src), Some(tgt)) => src != tgt,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    };
    let prelude = java_default_target_prelude(p, &parsed.source, target_package.as_deref());

    // Source edits: delete each class from highest byte to lowest so
    // earlier byte offsets stay valid.
    selected.sort_by_key(|c| std::cmp::Reverse(c.item.byte_start));
    let mut source_edits = Vec::new();
    for class_item in &selected {
        source_edits.push(TextEdit {
            byte_start: class_item.item.leading_trivia_start,
            byte_end: class_item.item.byte_end,
            replacement: String::new(),
        });
    }

    // Gap 10: strip `private` and `static` modifiers on each moved class
    // becoming top-level. Promote to `public` on cross-package extracts so
    // the source's remaining qualified references still resolve. Same-package
    // extracts keep the default-package visibility — operator can widen
    // afterwards if needed.
    let mut extracted_content = Vec::new();
    for class_item in &selected {
        let raw = parsed
            .source
            .get(class_item.item.leading_trivia_start..class_item.item.byte_end)
            .ok_or_else(|| anyhow!("invalid nested class range for `{}`", class_item.item.name.as_deref().unwrap_or("(unnamed)")))?;
        let rewritten = rewrite_top_level_class_modifiers(raw, cross_package)?;
        extracted_content.push(rewritten);
    }
    extracted_content.reverse();

    let mut target_content = format!("{}{}\n", prelude, extracted_content.join("\n\n"));

    // Gap 12: qualify references to OTHER source-class inner types in the
    // moved body. Same machinery as extract_java_class's inner-type
    // qualification (Gap 7). Bare `Mode` (where Mode is a sibling inner
    // enum of the source class) needs to become `Outer.Mode` on the new
    // top-level target, and cross-package targets need
    // `import <source-pkg>.<SourceClass>;` so the qualifier resolves.
    let class_node_opt = find_first_class_declaration(parsed.tree.root_node());
    let moved_names: HashSet<String> = selected
        .iter()
        .filter_map(|c| c.item.name.clone())
        .collect();
    if let Some(class_node) = class_node_opt {
        let source_class_name = java_class_name(class_node, &parsed.source);
        let inner_type_decls =
            collect_sibling_inner_type_names(class_node, &parsed.source, &moved_names);
        if !inner_type_decls.is_empty() {
            let referenced =
                qualify_inner_type_refs_in_text(&mut target_content, &inner_type_decls, &source_class_name);
            if !referenced.is_empty() && cross_package {
                if let Some(src_pkg) = source_package.as_deref() {
                    let fqcn = format!("{src_pkg}.{source_class_name}");
                    target_content = java_inject_import(&target_content, &fqcn);
                }
            }
        }
    }

    // Gap 11: cross-package extracts leave sibling-inner / outer-method
    // references to the moved class in source — they used to resolve via
    // the nested scope and now need `import <target-pkg>.<MovedClass>;`
    // for the bare name to bind to the new top-level class.
    if cross_package {
        if let Some(target_pkg) = target_package.as_deref() {
            let deletion_ranges: Vec<(usize, usize)> = selected
                .iter()
                .map(|c| (c.item.leading_trivia_start, c.item.byte_end))
                .collect();
            for class_item in &selected {
                let Some(name) = class_item.item.name.as_deref() else {
                    continue;
                };
                if !source_references_simple_name_outside(&parsed, name, &deletion_ranges) {
                    continue;
                }
                let fqcn = if target_pkg.is_empty() {
                    name.to_string()
                } else {
                    format!("{target_pkg}.{name}")
                };
                if let Some(import_edit) = java_source_import_edit(&parsed.source, &fqcn) {
                    source_edits.push(import_edit);
                }
            }
        }
    }

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
        // Gap 10: the previous validations: vec![] meant tree-sitter
        // didn't even fire on the target. Wire up parse-validation on
        // both files so future regressions surface at plan time.
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
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

/// Gap 10: rewrite the modifier prefix of a top-level Java class
/// declaration that was just extracted from a nested position.
///
/// - Strip `private` and `static` tokens. A top-level Java class can
///   neither be private nor static; javac rejects both.
/// - When `cross_package` is true and the declaration has no explicit
///   `public` modifier, inject `public ` before the existing modifiers
///   (or before the `class`/`interface`/`record`/`enum` keyword if no
///   modifiers exist). Same-package extracts keep package-default
///   visibility — operator widens afterwards if needed.
/// - `final`, `abstract`, `sealed`, `non-sealed`, annotations, and
///   anything else passes through untouched.
fn rewrite_top_level_class_modifiers(raw: &str, cross_package: bool) -> Result<String> {
    let tree = parse_source("java", raw)?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    let decl = root.named_children(&mut cursor).find(|n| {
        matches!(
            n.kind(),
            "class_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "annotation_type_declaration"
        )
    });
    let Some(decl) = decl else {
        // No top-level declaration parsed — return raw text unchanged
        // so the validator catches the syntax error rather than us
        // silently dropping the move.
        return Ok(raw.to_string());
    };

    let mut modifiers_node = None;
    let mut keyword_node = None;
    {
        let mut c = decl.walk();
        for child in decl.children(&mut c) {
            match child.kind() {
                "modifiers" => modifiers_node = Some(child),
                "class" | "interface" | "record" | "enum" => {
                    keyword_node = Some(child);
                    break;
                }
                _ => {}
            }
        }
    }

    let bytes = raw.as_bytes();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut has_public = false;

    if let Some(mods) = modifiers_node {
        let mut c = mods.walk();
        for tok in mods.children(&mut c) {
            match tok.kind() {
                "private" | "static" => {
                    let start = tok.start_byte();
                    let mut end = tok.end_byte();
                    // Absorb a single trailing space so the next
                    // modifier or keyword butts up cleanly. Don't
                    // absorb newlines — annotations on their own line
                    // shouldn't lose their separator.
                    if end < bytes.len() && bytes[end] == b' ' {
                        end += 1;
                    }
                    edits.push((start, end, String::new()));
                }
                "public" => has_public = true,
                _ => {}
            }
        }
    }

    if cross_package && !has_public {
        let insert_at = modifiers_node
            .map(|n| n.start_byte())
            .or_else(|| keyword_node.map(|n| n.start_byte()))
            .unwrap_or_else(|| decl.start_byte());
        edits.push((insert_at, insert_at, "public ".to_string()));
    }

    // Gap 13: when the class header gets widened, the constructor
    // declarations inside must follow — otherwise a `private Foo(...)`
    // ctor blocks callers from constructing the new top-level class
    // (`error: Foo() has private access in Foo` on cross-package
    // `new Foo()` sites; `error: Foo() is not visible` for same-pkg
    // when the class was promoted to package-default). Walk the class
    // body for constructor_declaration nodes and apply the same strip-
    // private + maybe-inject-public dance we did on the class header.
    if let Some(body) = decl.child_by_field_name("body") {
        let mut c = body.walk();
        for child in body.named_children(&mut c) {
            if child.kind() != "constructor_declaration" {
                continue;
            }
            edits.extend(constructor_visibility_edits(child, bytes, cross_package));
        }
    }

    if edits.is_empty() {
        return Ok(raw.to_string());
    }
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = raw.to_string();
    for (start, end, repl) in edits {
        out.replace_range(start..end, &repl);
    }
    Ok(out)
}

/// Gap 13: compute visibility edits for one constructor inside a
/// just-promoted top-level class. Strips `private`; on cross-package
/// extracts also injects `public ` if neither `public` nor `protected`
/// already exists. `protected` is left alone — operator may have
/// chosen it deliberately and widening it to `public` could escalate
/// API surface unintentionally.
fn constructor_visibility_edits(
    ctor: Node<'_>,
    bytes: &[u8],
    cross_package: bool,
) -> Vec<(usize, usize, String)> {
    let mut edits = Vec::new();
    let mut ctor_modifiers = None;
    let mut name_node = None;
    {
        let mut c = ctor.walk();
        for child in ctor.children(&mut c) {
            match child.kind() {
                "modifiers" => ctor_modifiers = Some(child),
                "identifier" => {
                    if name_node.is_none() {
                        name_node = Some(child);
                    }
                }
                _ => {}
            }
        }
    }
    let mut has_public_or_protected = false;
    if let Some(mods) = ctor_modifiers {
        let mut c = mods.walk();
        for tok in mods.children(&mut c) {
            match tok.kind() {
                "private" => {
                    let start = tok.start_byte();
                    let mut end = tok.end_byte();
                    if end < bytes.len() && bytes[end] == b' ' {
                        end += 1;
                    }
                    edits.push((start, end, String::new()));
                }
                "public" | "protected" => has_public_or_protected = true,
                _ => {}
            }
        }
    }
    if cross_package && !has_public_or_protected {
        let insert_at = ctor_modifiers
            .map(|n| n.start_byte())
            .or_else(|| name_node.map(|n| n.start_byte()))
            .unwrap_or_else(|| ctor.start_byte());
        edits.push((insert_at, insert_at, "public ".to_string()));
    }
    edits
}

/// Gap 12: collect simple names of nested classes / interfaces /
/// records / enums declared directly inside the source class body,
/// EXCLUDING the items being moved (we don't want to rewrite a moved
/// class's own declaration name to `Outer.MovedClass`).
fn collect_sibling_inner_type_names(
    class_node: Node<'_>,
    source: &str,
    moved_names: &HashSet<String>,
) -> BTreeMap<String, ()> {
    let mut map = BTreeMap::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return map;
    };
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ) {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        if moved_names.contains(name) {
            continue;
        }
        map.insert(name.to_string(), ());
    }
    map
}

/// Gap 12: walk the assembled target text for `type_identifier` nodes
/// and uppercase-receiver `identifier` nodes that match a sibling
/// inner-type name, and rewrite each to `<SourceClass>.<InnerType>`.
/// Already-qualified references inside `scoped_type_identifier` are
/// left alone. Returns the set of inner-type names actually rewritten
/// (used to decide whether to inject the source-class import).
fn qualify_inner_type_refs_in_text(
    target_text: &mut String,
    inner_type_decls: &BTreeMap<String, ()>,
    source_class_name: &str,
) -> BTreeSet<String> {
    let mut referenced = BTreeSet::new();
    let Ok(tree) = parse_source("java", target_text) else {
        return referenced;
    };
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // Pass 1: type_identifier nodes (`InnerType` as a type position).
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            stack.push(ch);
        }
        if node.kind() != "type_identifier" {
            continue;
        }
        let Ok(text) = node.utf8_text(target_text.as_bytes()) else {
            continue;
        };
        if !inner_type_decls.contains_key(text) {
            continue;
        }
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
        referenced.insert(text.to_string());
    }
    // Pass 2: uppercase-receiver identifier nodes
    // (`InnerEnum.VALUE` or `InnerType.staticCall()`).
    let mut stack2 = vec![tree.root_node()];
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
        let Ok(text) = receiver.utf8_text(target_text.as_bytes()) else {
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
        referenced.insert(text.to_string());
    }
    edits.sort_by_key(|e| e.0);
    edits.dedup_by_key(|e| e.0);
    // Apply in reverse order to preserve earlier byte offsets.
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (start, end, repl) in edits {
        target_text.replace_range(start..end, &repl);
    }
    referenced
}

/// Gap 11: returns true when the source file has at least one
/// `type_identifier` or `identifier` token matching `name` outside any
/// of the deletion ranges. Used to decide whether the source needs
/// `import <target-pkg>.<MovedClass>;` post-move so sibling-inner
/// references still resolve.
fn source_references_simple_name_outside(
    parsed: &ParsedSource,
    name: &str,
    deletion_ranges: &[(usize, usize)],
) -> bool {
    let bytes = parsed.source.as_bytes();
    let mut stack = vec![parsed.tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for ch in node.named_children(&mut c) {
            stack.push(ch);
        }
        // type_identifier covers bare `MovedClass` in type position
        // (variable decl, parameter, return type, generic argument).
        // identifier covers uppercase-receiver call/field-access.
        if !matches!(node.kind(), "type_identifier" | "identifier") {
            continue;
        }
        let s = node.start_byte();
        let e = node.end_byte();
        if e - s != name.len() {
            continue;
        }
        if &bytes[s..e] != name.as_bytes() {
            continue;
        }
        // Skip nodes inside any deletion range.
        if deletion_ranges
            .iter()
            .any(|(ds, de)| s >= *ds && e <= *de)
        {
            continue;
        }
        // Skip the moved class's own declaration site (its name_node).
        // declaration name positions are handled by the deletion-range
        // check above (the whole declaration is inside the deletion).
        return true;
    }
    false
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

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct PromotedCapture {
    pub(crate) name: String,
    pub(crate) type_name: String,
    pub(crate) source_visibility: String,
    pub(crate) source_mutable: bool,
}

#[derive(Debug, Default)]
pub(crate) struct InnerClassRefAnalysis {
    pub(crate) captures: Vec<PromotedCapture>,
    pub(crate) outer_field_writes: Vec<String>,
    pub(crate) outer_method_calls: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct InnerNewSite {
    pub(crate) args_open: usize,
    pub(crate) args_close: usize,
}

#[derive(Debug, Default)]
pub(crate) struct InnerClassUsageScan {
    pub(crate) new_sites: Vec<InnerNewSite>,
    pub(crate) non_new_sites: Vec<(usize, usize)>,
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

    let outer_class_node =
        find_first_class_declaration(parsed.tree.root_node()).ok_or_else(|| {
            anyhow!(
                "no outer class declaration found in {}",
                source_path.display()
            )
        })?;
    let outer_body = outer_class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("outer class has no body"))?;
    let inner_class_node = {
        let mut cursor = outer_body.walk();
        outer_body
            .named_children(&mut cursor)
            .find(|child| {
                matches!(child.kind(), "class_declaration" | "record_declaration")
                    && child
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
        file_creates: Vec::new(),
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

/// Walk an inner class body, classify every identifier/method-invocation
/// against the outer class's field and method declarations, and return:
/// - `captures`: outer fields the inner READS (post-shadow)
/// - `outer_field_writes`: outer fields the inner WRITES (refusal trigger)
/// - `outer_method_calls`: outer-class methods the inner calls (refusal)
///
/// `this.field` is never treated as an outer capture (anonymous-class
/// boundary is the only place that re-binds `this`; lambdas inherit
/// enclosing `this`). `OuterClass.this.field` is always an outer capture.
pub(crate) fn analyze_inner_class_outer_refs(
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
                if let Some(captured_name) = classify_outer_field_access(
                    left,
                    parsed,
                    outer_fields,
                    inner_field_set,
                    &outer_name,
                    inside_anonymous,
                ) {
                    writes.insert(captured_name);
                }
            }
            continue;
        }
        if node.kind() == "update_expression" {
            // `field++`, `--field`, etc. — treat as write
            let mut ucur = node.walk();
            for c in node.named_children(&mut ucur) {
                if let Some(captured) = classify_outer_field_access(
                    c,
                    parsed,
                    outer_fields,
                    inner_field_set,
                    &outer_name,
                    inside_anonymous,
                ) {
                    writes.insert(captured);
                }
            }
            continue;
        }

        // Bare identifier read or `Outer.this.field`. Classify as outer
        // capture if it resolves to an outer field.
        if let Some(captured_name) = classify_outer_field_access(
            node,
            parsed,
            outer_fields,
            inner_field_set,
            &outer_name,
            inside_anonymous,
        ) {
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
    InnerClassRefAnalysis {
        captures: captures.into_values().collect(),
        outer_field_writes: writes.into_iter().collect(),
        outer_method_calls: methods.into_iter().collect(),
    }
}

/// If `node` is an outer-field access — bare `identifier` matching an outer
/// field name (post inner-field shadow check) OR `OuterClass.this.field` —
/// return the field name. Otherwise return `None`.
pub(crate) fn classify_outer_field_access(
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
                "method_invocation"
                    if parent.child_by_field_name("name").map(|c| c.id()) == Some(node.id()) => {
                        return None;
                    }
                "scoped_identifier"
                | "scoped_type_identifier"
                | "type_identifier"
                | "generic_type" => return None,
                "field_access"
                    // `something.field` — `field` part is consumed by the
                    // field_access classifier below; skip.
                    if parent.child_by_field_name("field").map(|c| c.id()) == Some(node.id()) => {
                        return None;
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
pub(crate) fn is_outer_qualified_this(node: Node<'_>, source: &str, outer_name: &str) -> bool {
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
pub(crate) fn constructor_has_this_chain(ctor: Node<'_>, source: &str) -> bool {
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

/// Scan the source AST for references to `inner_name` outside the inner
/// class's own declaration. Returns `new_sites` (positions of the args
/// list for each `new <Inner>(...)` instantiation) and `non_new_sites`
/// (every other type-position reference, which v1 refuses to handle).
pub(crate) fn scan_source_for_inner_class_uses(
    parsed: &ParsedSource,
    inner_name: &str,
) -> InnerClassUsageScan {
    let mut scan = InnerClassUsageScan::default();
    // Find the inner class node's range so we can skip it while walking.
    let inner_range = find_node(parsed.tree.root_node(), |node| {
        matches!(node.kind(), "class_declaration" | "record_declaration")
            && node
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
                    let is_new_type_slot = node.parent().and_then(|p| {
                        if p.kind() == "object_creation_expression" {
                            p.child_by_field_name("type").map(|t| t.id())
                        } else {
                            None
                        }
                    }) == Some(node.id());
                    if !is_new_type_slot {
                        scan.non_new_sites
                            .push((node.start_byte(), node.end_byte()));
                    }
                }
            }
        }
        if node.kind() == "method_reference" {
            let mut mcur = node.walk();
            if let Some(qualifier) = node.named_children(&mut mcur).next() {
                if let Ok(text) = qualifier.utf8_text(parsed.source.as_bytes()) {
                    if text.trim() == inner_name {
                        scan.non_new_sites
                            .push((qualifier.start_byte(), qualifier.end_byte()));
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
pub(crate) fn render_promoted_class(
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

pub(crate) fn synthesize_promoted_ctor(
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
pub(crate) fn extend_existing_ctor_with_captures(
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
                            insert_at_local = first_stmt.end_byte() - "class __Tmp { ".len();
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
pub(crate) fn java_node_leading_trivia_start(node: Node<'_>, source: &str) -> usize {
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

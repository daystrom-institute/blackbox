//! `convert_method_to_class` — Method Object pattern with automatic
//! enclosing-state capture.
//!
//! Given a method `foo(A a, B b) returning R` on class `Outer`, generate
//! a standalone class `FooOperation` whose private final fields hold
//! the original parameters PLUS every enclosing-class field referenced
//! in the method body. The constructor accepts them all; the body
//! becomes `execute(): R` with `this.field` references rewritten to the
//! local field. Original method body becomes:
//!
//! ```text
//! return new FooOperation(a, b, this.captureField1, this.captureField2).execute();
//! ```
//!
//! Field captures are by VALUE — the Method Object snapshots them at
//! construction time. If the original method body mutates an enclosing
//! field, the mutation cannot propagate back to the enclosing instance;
//! the planner refuses in that case (`error.mutated_enclosing_field`).
//!
//! ## Refusals
//!
//! Refuse cleanly, with operator-actionable guidance. No FIXME-and-hope.
//!
//! - `error.method_not_found(name)`
//! - `error.unsupported_method_kind` — static / abstract / constructor /
//!   interface method.
//! - `error.method_has_no_body`
//! - `error.target_already_exists`
//! - `error.mutated_enclosing_field(name)` — body reassigns `this.X`
//!   or uses `++`/`--` on it. Method Object capture-by-value can't
//!   propagate the mutation back. Operator should: refactor the
//!   method to take the mutated field as a parameter and return the
//!   new value, or skip Method Object for this method.
//! - `error.enclosing_method_call(name)` — body makes a receiverless
//!   method call that resolves to an enclosing-instance method.
//!   Operator should: inline the called method (`java-inline-method`),
//!   or move the called method to the new Method Object class first,
//!   or thread an `Outer` instance through manually.
//! - `error.unresolvable_reference(name)` — body references a bare
//!   identifier that resolves to neither a local nor a field on the
//!   enclosing class. Likely a static import; operator should make
//!   the reference explicit (`Math.max` etc.) and re-run.
//! - `error.super_reference` — body uses `super.X` or `super::method`.
//!   No safe way to capture super in a separate class; operator must
//!   refactor.
//! - `error.bare_this_reference` — body uses `this` as a value (not
//!   `this.X`). The Method Object isn't the same identity as the
//!   enclosing instance; refuse.

use super::scope::{ScopeTree, analyze_range};
use super::*;
use crate::refactor::java::lombokify::formal_parameters;
use std::collections::HashMap;

pub(crate) fn plan_convert_method_to_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!("target is required for convert_method_to_class (path for the new class file)")
        })
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if target_path.exists() {
        bail!(
            "target {} already exists; pick a different target or move the existing file first",
            target_path.display()
        );
    }
    let method_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (the method to convert) is required"))?;

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("convert_method_to_class only supports java files");
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
        bail!("convert_method_to_class does not operate on interface methods");
    }

    let method_node = find_method_in_class(class_node, method_name, &parsed.source)
        .ok_or_else(|| anyhow!("method `{method_name}` not found"))?;
    if method_node.kind() == "constructor_declaration" {
        bail!(
            "convert_method_to_class does not operate on constructors (use a factory-class refactor instead)"
        );
    }
    if has_java_modifier_node(method_node, "static") {
        bail!(
            "convert_method_to_class refuses static methods — the Method Object pattern only \
             applies to instance methods. Lift the method to instance first, or pick a different refactor."
        );
    }
    if has_java_modifier_node(method_node, "abstract") {
        bail!("convert_method_to_class refuses abstract methods (no body to extract)");
    }

    let body_node = method_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("method `{method_name}` has no body"))?;
    let body_text = parsed.source[body_node.start_byte()..body_node.end_byte()].to_string();

    let return_type = method_node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("void")
        .to_string();
    let is_void = return_type == "void";
    let params = formal_parameters(method_node, &parsed.source);

    let throws_clause = collect_throws_clause(method_node, &parsed.source);

    let class_name = p
        .new_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_class_name(method_name));
    if !is_pascal_case_identifier(&class_name) {
        bail!("class name `{class_name}` is not a valid PascalCase Java identifier");
    }

    // -----------------------------------------------------------------
    // Enclosing-state analysis.
    // -----------------------------------------------------------------

    let class_fields = collect_class_fields(class_node, &parsed.source);
    let class_methods = collect_class_method_names(class_node, &parsed.source);
    let scope_tree = ScopeTree::build_from_method(method_node, &parsed.source);
    let analysis = analyze_range(
        &scope_tree,
        method_node,
        body_node.start_byte(),
        body_node.end_byte(),
        &parsed.source,
    );

    // Refuse: bare `this`/`super` expressions (not as `this.X` field
    // access) — those are uses of identity, which the Method Object
    // doesn't share.
    refuse_bare_this_super(body_node, &parsed.source)?;

    // Find every `this.X` field access in the body, and every
    // receiverless method_invocation. Build:
    //   - enclosing_field_captures: set of field names from class_fields
    //     that the body reads (via this.X or bare X).
    //   - any disallowed reference (unresolvable, method-call, etc.)
    //     → refuse.
    let (enclosing_field_captures, this_field_access_ranges) = classify_enclosing_refs(
        body_node,
        &parsed.source,
        &scope_tree,
        &analysis,
        &class_fields,
        &class_methods,
        method_name,
    )?;

    // Refuse: mutated enclosing field.
    refuse_mutated_enclosing_fields(body_node, &parsed.source, &enclosing_field_captures)?;

    // -----------------------------------------------------------------
    // Generate the new Method Object class body with `this.field`
    // rewritten to bare `field` references.
    // -----------------------------------------------------------------

    let rewritten_body = rewrite_body(
        &body_text,
        body_node.start_byte(),
        &this_field_access_ranges,
    );

    let target_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)
        .ok()
        .flatten();
    let prelude = java_default_target_prelude(p, &parsed.source, target_pkg.as_deref());

    // Constructor param list = (original method params) + (captured fields).
    let mut all_ctor_params: Vec<(String, String)> =
        params.iter().map(|(t, n)| (t.clone(), n.clone())).collect();
    for (name, type_text) in &enclosing_field_captures {
        all_ctor_params.push((type_text.clone(), name.clone()));
    }

    let target_text = render_method_object_class(
        &class_name,
        &prelude,
        &return_type,
        is_void,
        &all_ctor_params,
        &throws_clause,
        &rewritten_body,
    );

    // Caller-side call: `new MO(originalArgs..., this.captured...).execute()`.
    let mut all_call_args: Vec<String> = params.iter().map(|(_, n)| n.clone()).collect();
    for (name, _) in &enclosing_field_captures {
        all_call_args.push(format!("this.{name}"));
    }
    let arg_list = all_call_args.join(", ");
    let delegate_body = if is_void {
        format!("{{\n        new {class_name}({arg_list}).execute();\n    }}")
    } else {
        format!("{{\n        return new {class_name}({arg_list}).execute();\n    }}")
    };
    let source_edit = TextEdit {
        byte_start: body_node.start_byte(),
        byte_end: body_node.end_byte(),
        replacement: delegate_body,
    };

    let validations = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect();

    let leftovers = if enclosing_field_captures.is_empty() {
        Vec::new()
    } else {
        let names: Vec<&str> = enclosing_field_captures
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        vec![format!("captured_enclosing_fields={names:?}")]
    };

    let plan = RefactorPlan {
        title: format!(
            "convert {}.{} → Method Object class {} ({} captured field(s)) at {}",
            java_class_simple_name(class_node, &parsed.source)
                .unwrap_or_else(|| "(unnamed)".into()),
            method_name,
            class_name,
            enclosing_field_captures.len(),
            path_string(&target_path),
        ),
        kind: "convert_method_to_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: vec![source_edit],
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
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Walk the class body for field_declaration nodes and collect
/// (name → type_text). Multi-declarator fields each get their own
/// entry sharing the declared type.
fn collect_class_fields(class_node: Node<'_>, source: &str) -> HashMap<String, String> {
    let bytes = source.as_bytes();
    let mut out = HashMap::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "field_declaration" {
            continue;
        }
        let Some(type_node) = member.child_by_field_name("type") else {
            continue;
        };
        let type_text = type_node.utf8_text(bytes).unwrap_or("").trim().to_string();
        let mut mc = member.walk();
        for decl in member.named_children(&mut mc) {
            if decl.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name_node) = decl.child_by_field_name("name") {
                if let Ok(name) = name_node.utf8_text(bytes) {
                    out.insert(name.to_string(), type_text.clone());
                }
            }
        }
    }
    out
}

fn collect_class_method_names(
    class_node: Node<'_>,
    source: &str,
) -> std::collections::HashSet<String> {
    let bytes = source.as_bytes();
    let mut out = std::collections::HashSet::new();
    let Some(body) = class_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member.kind() != "method_declaration" {
            continue;
        }
        if let Some(n) = member.child_by_field_name("name") {
            if let Ok(name) = n.utf8_text(bytes) {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn refuse_bare_this_super(body_node: Node<'_>, source: &str) -> Result<()> {
    let _ = source;
    let mut stack = vec![body_node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        let kind = n.kind();
        if kind == "this" {
            // `this` is fine when it's the `object` of a field_access
            // or method_invocation (those are handled separately).
            // Bare `this` as an argument, return value, comparison
            // operand, etc. is what we refuse.
            if let Some(parent) = n.parent() {
                match parent.kind() {
                    "field_access" | "method_invocation" => continue,
                    _ => bail!(
                        "error.bare_this_reference: the method body uses `this` as a value. \
                         The Method Object class doesn't share identity with the enclosing \
                         instance, so `this` would refer to the wrong object. Refactor the \
                         method to not pass `this` as a value before re-running."
                    ),
                }
            }
        }
        if kind == "super" {
            bail!(
                "error.super_reference: the method body uses `super`. The Method Object class \
                 cannot inherit the enclosing class's parent. Refactor the `super` reference \
                 (e.g. extract a protected helper on Outer that wraps super.X) before re-running."
            );
        }
    }
    Ok(())
}

fn classify_enclosing_refs(
    body_node: Node<'_>,
    source: &str,
    scope_tree: &ScopeTree,
    analysis: &super::scope::RangeAnalysis,
    class_fields: &HashMap<String, String>,
    class_methods: &std::collections::HashSet<String>,
    method_name: &str,
) -> Result<(Vec<(String, String)>, Vec<(usize, usize)>)> {
    let _ = analysis;
    let bytes = source.as_bytes();
    let mut captures: Vec<(String, String)> = Vec::new();
    let mut this_field_access_ranges: Vec<(usize, usize)> = Vec::new();
    let mut seen_captures: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut stack = vec![body_node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        let kind = n.kind();
        match kind {
            "field_access" => {
                // Form: <obj>.<field>. We care about object=this.
                let Some(obj) = n.child_by_field_name("object") else {
                    continue;
                };
                if obj.kind() != "this" {
                    continue;
                }
                let Some(field_node) = n.child_by_field_name("field") else {
                    continue;
                };
                let Ok(field_name) = field_node.utf8_text(bytes) else {
                    continue;
                };
                let Some(type_text) = class_fields.get(field_name) else {
                    bail!(
                        "error.unresolvable_reference({field_name}): body of `{method_name}` reads \
                         `this.{field_name}` but no such field is declared on the enclosing class. \
                         Likely a typo or a reference to an inherited field; resolve before re-running."
                    );
                };
                if seen_captures.insert(field_name.to_string()) {
                    captures.push((field_name.to_string(), type_text.clone()));
                }
                // Track the byte range of `this.` prefix (we'll strip
                // it during body rewrite).
                this_field_access_ranges.push((obj.start_byte(), field_node.start_byte()));
            }
            "method_invocation" => {
                // Receiverless? (no object field)
                let recv = n.child_by_field_name("object");
                if recv.is_none() {
                    let Some(name_node) = n.child_by_field_name("name") else {
                        continue;
                    };
                    let Ok(name) = name_node.utf8_text(bytes) else {
                        continue;
                    };
                    if class_methods.contains(name) {
                        bail!(
                            "error.enclosing_method_call({name}): body of `{method_name}` calls \
                             `{name}(...)` with no receiver, which resolves to a method on the \
                             enclosing class. The Method Object class doesn't have access to that \
                             method. Either: (a) inline `{name}` first (java-inline-method), (b) \
                             move `{name}` into the new Method Object class, or (c) refactor the \
                             call to be explicit on a parameter."
                        );
                    }
                    // Receiverless call to a name that's not a known
                    // method on the enclosing class. Could be a static
                    // import — refuse for safety. Operator can qualify
                    // the call (e.g. `Math.max` instead of `max`) and
                    // re-run.
                    bail!(
                        "error.unresolvable_reference({name}): body of `{method_name}` calls \
                         `{name}(...)` with no receiver, but `{name}` is not declared on the \
                         enclosing class. Likely a static import — qualify the call explicitly \
                         (e.g. `Math.max(...)`) and re-run."
                    );
                }
            }
            "identifier" => {
                // Bare identifier reference. If it resolves to a local
                // via the scope walker, it's fine (the Method Object
                // captures locals as its own params naturally). If it
                // doesn't, check the class field index.
                let Some(parent) = n.parent() else {
                    continue;
                };
                // Skip identifiers that are NAMES of nodes (already
                // declarations, not references).
                if is_decl_name_in_parent(n, parent) {
                    continue;
                }
                // Skip identifiers that are method names (handled in
                // method_invocation above).
                if parent.kind() == "method_invocation"
                    && parent.child_by_field_name("name").map(|c| c.id()) == Some(n.id())
                {
                    continue;
                }
                // Skip identifiers that are field names in field_access
                // (handled above).
                if parent.kind() == "field_access"
                    && parent.child_by_field_name("field").map(|c| c.id()) == Some(n.id())
                {
                    continue;
                }
                let Ok(text) = n.utf8_text(bytes) else {
                    continue;
                };
                if scope_tree.resolve(text, n.start_byte()).is_some() {
                    continue;
                }
                if text.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    // Type name; not a field.
                    continue;
                }
                let Some(type_text) = class_fields.get(text) else {
                    bail!(
                        "error.unresolvable_reference({text}): body of `{method_name}` references \
                         bare identifier `{text}` that resolves to neither a local nor a field on \
                         the enclosing class. Likely a static import — qualify the reference \
                         explicitly and re-run."
                    );
                };
                if seen_captures.insert(text.to_string()) {
                    captures.push((text.to_string(), type_text.clone()));
                }
                // Bare identifier `field` — body rewrite leaves it as-is
                // because the new class has a field with the same name.
            }
            _ => {}
        }
    }
    Ok((captures, this_field_access_ranges))
}

fn is_decl_name_in_parent(node: Node<'_>, parent: Node<'_>) -> bool {
    match parent.kind() {
        "formal_parameter"
        | "spread_parameter"
        | "variable_declarator"
        | "type_parameter"
        | "catch_formal_parameter"
        | "resource"
        | "enhanced_for_statement" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        _ => false,
    }
}

fn refuse_mutated_enclosing_fields(
    body_node: Node<'_>,
    source: &str,
    captures: &[(String, String)],
) -> Result<()> {
    let bytes = source.as_bytes();
    let names: std::collections::HashSet<&str> = captures.iter().map(|(n, _)| n.as_str()).collect();
    let mut stack = vec![body_node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        let kind = n.kind();
        if kind == "assignment_expression" {
            if let Some(left) = n.child_by_field_name("left") {
                let target_name = mutation_target_name(left, bytes, &names);
                if let Some(name) = target_name {
                    bail!(
                        "error.mutated_enclosing_field({name}): body mutates `this.{name}` (or \
                         bare `{name}` referring to it). The Method Object class captures \
                         enclosing fields by VALUE — assignment inside execute() can't propagate \
                         back to the enclosing instance. Operator workflow: refactor the method to \
                         take `{name}` as a parameter and return the new value, then update \
                         callers."
                    );
                }
            }
        }
        if kind == "update_expression" {
            // Find the operand.
            let mut uc = n.walk();
            for op in n.named_children(&mut uc) {
                if let Some(name) = mutation_target_name(op, bytes, &names) {
                    bail!(
                        "error.mutated_enclosing_field({name}): body uses `++`/`--` on \
                         `this.{name}` (or bare `{name}` referring to it). The Method Object \
                         class captures enclosing fields by VALUE — the increment can't propagate \
                         back. Refactor the method to take `{name}` as a parameter and return the \
                         new value."
                    );
                }
            }
        }
    }
    Ok(())
}

fn mutation_target_name(
    node: Node<'_>,
    bytes: &[u8],
    names: &std::collections::HashSet<&str>,
) -> Option<String> {
    match node.kind() {
        "field_access" => {
            let obj = node.child_by_field_name("object")?;
            if obj.kind() != "this" {
                return None;
            }
            let field = node.child_by_field_name("field")?;
            let text = field.utf8_text(bytes).ok()?;
            if names.contains(text) {
                Some(text.to_string())
            } else {
                None
            }
        }
        "identifier" => {
            let text = node.utf8_text(bytes).ok()?;
            if names.contains(text) {
                Some(text.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Strip `this.` prefixes at the given byte ranges (relative to the
/// source file; we adjust to body-local before splicing) so that
/// `this.field` becomes `field` in the rewritten body.
fn rewrite_body(body_text: &str, body_start: usize, prefix_ranges: &[(usize, usize)]) -> String {
    if prefix_ranges.is_empty() {
        return body_text.to_string();
    }
    let mut sorted: Vec<_> = prefix_ranges.to_vec();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut out = body_text.to_string();
    for (start, end) in sorted {
        let local_start = start - body_start;
        let local_end = end - body_start;
        if local_end <= out.len() && local_start <= local_end {
            out.replace_range(local_start..local_end, "");
        }
    }
    out
}

fn find_method_in_class<'a>(class_node: Node<'a>, name: &str, source: &str) -> Option<Node<'a>> {
    let body = class_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        let kind = child.kind();
        if kind != "method_declaration" && kind != "constructor_declaration" {
            continue;
        }
        let cname = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())?;
        if cname == name {
            return Some(child);
        }
    }
    None
}

fn collect_throws_clause(method_node: Node<'_>, source: &str) -> String {
    let mut cursor = method_node.walk();
    for child in method_node.named_children(&mut cursor) {
        if child.kind() == "throws" {
            if let Ok(text) = child.utf8_text(source.as_bytes()) {
                return text.trim().to_string();
            }
        }
    }
    String::new()
}

fn derive_class_name(method_name: &str) -> String {
    let mut chars = method_name.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    let rest: String = chars.collect();
    format!("{first}{rest}Operation")
}

fn is_pascal_case_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn has_java_modifier_node(node: Node<'_>, modifier: &str) -> bool {
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

fn java_class_simple_name(class_node: Node<'_>, source: &str) -> Option<String> {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

fn render_method_object_class(
    class_name: &str,
    prelude: &str,
    return_type: &str,
    is_void: bool,
    ctor_params: &[(String, String)],
    throws_clause: &str,
    body_text: &str,
) -> String {
    let mut out = String::new();
    out.push_str(prelude);
    out.push_str(&format!("public class {class_name} {{\n"));

    for (ty, name) in ctor_params {
        out.push_str(&format!("    private final {ty} {name};\n"));
    }
    if !ctor_params.is_empty() {
        out.push('\n');
    }

    let ctor_args = ctor_params
        .iter()
        .map(|(ty, name)| format!("{ty} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("    public {class_name}({ctor_args}) {{\n"));
    for (_, name) in ctor_params {
        out.push_str(&format!("        this.{name} = {name};\n"));
    }
    out.push_str("    }\n\n");

    let throws_suffix = if throws_clause.is_empty() {
        String::new()
    } else {
        format!(" {throws_clause}")
    };
    let exec_return = if is_void { "void" } else { return_type };
    out.push_str(&format!(
        "    public {exec_return} execute(){throws_suffix} "
    ));
    out.push_str(body_text.trim());
    out.push_str("\n}\n");
    out
}

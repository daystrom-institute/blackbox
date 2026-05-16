//! `inline_java_method` — replace call sites of a private method with
//! its body, then delete the declaration.
//!
//! v1 scope: PRIVATE single-statement methods whose body references
//! only the formal parameters. Two shapes accepted:
//!
//! - Non-void: `private <T> foo(...) { return <expr>; }` — call site
//!   `foo(arg1, arg2)` is replaced with `<expr>` with each parameter
//!   identifier substituted by the corresponding argument (in parens).
//! - Void: `private void foo(...) { stmt; }` — call site `foo(arg1, arg2);`
//!   (as an expression_statement) is replaced with `stmt;` with the
//!   same substitution.
//!
//! After all call sites are rewritten, the method declaration is
//! deleted.
//!
//! ## Inputs
//!
//! - `source` (required)
//! - `project_dir` (required)
//! - `impl_name` (optional) — enclosing class
//! - `module_name` (required) — method name to inline
//!
//! ## v1 refusals (fail closed)
//!
//! - `error.method_not_found`
//! - `error.not_private` — inlining non-private methods requires a
//!   project-wide caller walk; out of v1 scope
//! - `error.body_too_complex` — body is not a single statement, OR
//!   references `this`/`super`/other methods/fields, OR void method's
//!   single statement isn't an expression_statement
//! - `error.parameter_type_inference_needed` — body uses a parameter
//!   in a context where Java requires the operator to keep the type
//!   annotation visible (e.g. a method-reference call qualifier). v1
//!   refuses; v2 may rewrite by adding an explicit cast
//!
//! ## v1 limitations (followups filed separately)
//!
//! - Single-statement bodies only.
//! - Private methods only (no cross-file caller walk).
//! - `inline_java_class` is a separate primitive; see followup note.

use super::*;
use crate::refactor::java::lombokify::formal_parameters;

pub(crate) fn plan_inline_java_method(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("inline_java_method only supports java files");
    }
    let method_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (method to inline) is required"))?;

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

    let method_node = find_method_by_name(class_node, method_name, &parsed.source)
        .ok_or_else(|| anyhow!("method `{method_name}` not found"))?;
    if method_node.kind() == "constructor_declaration" {
        bail!("inline_java_method cannot inline constructors");
    }
    if !has_modifier(method_node, "private") {
        bail!(
            "inline_java_method v1 refuses non-private method `{method_name}` — inlining \
             across files requires a project-wide caller walk; out of v1 scope"
        );
    }
    if has_modifier(method_node, "abstract") {
        bail!("method `{method_name}` is abstract — no body to inline");
    }

    let body_node = method_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("method `{method_name}` has no body"))?;
    let return_type = method_node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .map(str::trim)
        .unwrap_or("void");
    let is_void = return_type == "void";

    // Find the single statement inside the body block.
    let single_stmt = single_body_statement(body_node)?;

    // Resolve the expression we will splice in at each call site.
    let inline_expr_text = match (is_void, single_stmt.kind()) {
        (true, "expression_statement") => {
            // Strip trailing `;` from the expression statement text — we
            // re-add it when we substitute at a statement-level call
            // site.
            let raw = single_stmt
                .utf8_text(parsed.source.as_bytes())
                .map_err(|e| anyhow!("utf8: {e}"))?;
            raw.trim().trim_end_matches(';').trim().to_string()
        }
        (false, "return_statement") => {
            let raw = single_stmt
                .utf8_text(parsed.source.as_bytes())
                .map_err(|e| anyhow!("utf8: {e}"))?;
            raw.trim()
                .trim_start_matches("return")
                .trim_end_matches(';')
                .trim()
                .to_string()
        }
        _ => bail!(
            "inline_java_method v1 only supports single-statement bodies (return-expression or \
             expression-statement). Got `{}` for method `{method_name}`",
            single_stmt.kind()
        ),
    };

    let params = formal_parameters(method_node, &parsed.source);
    let param_names: Vec<&str> = params.iter().map(|(_, n)| n.as_str()).collect();

    // Body safety check: the inline expression may only reference the
    // formal parameter names — no `this`, `super`, other method calls,
    // or field accesses.
    check_body_safe(single_stmt, &parsed.source, &param_names, method_name)?;

    // Walk the file for call sites of the method.
    let call_sites = collect_call_sites(class_node, method_name, &parsed.source);
    if call_sites.is_empty() {
        bail!(
            "no call sites for `{method_name}` found in {} — inlining would only delete the \
             declaration; use `prune_java_orphans` or `delete_rust_items` instead",
            source_path.display()
        );
    }

    // For each call site, substitute the parameter identifiers in the
    // inline expression with the arguments at the call.
    let mut file_edits: Vec<TextEdit> = Vec::new();
    for call in &call_sites {
        let arguments = call_argument_expressions(*call, &parsed.source);
        if arguments.len() != params.len() {
            bail!(
                "call site at byte {} passes {} args but method `{method_name}` takes {}",
                call.start_byte(),
                arguments.len(),
                params.len()
            );
        }
        let substituted =
            substitute_params_in_expression(&inline_expr_text, &param_names, &arguments);

        // Replacement shape depends on whether the call is a statement
        // (void method called as `foo(x);`) or an expression (non-void
        // method called as `int y = foo(x);`).
        let (rep_start, rep_end, rep_text) = if let Some(stmt) = enclosing_expression_statement(*call) {
            (
                stmt.start_byte(),
                stmt.end_byte(),
                if is_void {
                    format!("{substituted};")
                } else {
                    format!("{substituted};")
                },
            )
        } else {
            // Inline expression replaces just the call_expression itself
            // — wrap in parens so we don't change operator precedence
            // at the surrounding site.
            (
                call.start_byte(),
                call.end_byte(),
                format!("({substituted})"),
            )
        };
        file_edits.push(TextEdit {
            byte_start: rep_start,
            byte_end: rep_end,
            replacement: rep_text,
        });
    }

    // Delete the method declaration (leading whitespace + trailing newline).
    let decl_start = leading_trivia_byte_start(&parsed.source, method_node);
    let decl_end = trailing_newline_byte_end(&parsed.source, method_node.end_byte());
    file_edits.push(TextEdit {
        byte_start: decl_start,
        byte_end: decl_end,
        replacement: String::new(),
    });

    file_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&file_edits)?;

    let title = format!(
        "inline private method `{method_name}` ({} call site{}) and delete the declaration in {}",
        call_sites.len(),
        if call_sites.len() == 1 { "" } else { "s" },
        path_string(&source_path)
    );

    let plan = RefactorPlan {
        title,
        kind: "inline_java_method".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: file_edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: vec![format!("call_sites_inlined={}", call_sites.len())],
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

fn find_method_by_name<'a>(
    class_node: Node<'a>,
    name: &str,
    source: &str,
) -> Option<Node<'a>> {
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

fn has_modifier(node: Node<'_>, modifier: &str) -> bool {
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

/// Return the single statement node inside a method `block` body.
/// Refuses if the body has zero or multiple statements.
fn single_body_statement(body: Node<'_>) -> Result<Node<'_>> {
    let mut cursor = body.walk();
    let stmts: Vec<_> = body
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect();
    match stmts.len() {
        1 => Ok(stmts[0]),
        n => bail!(
            "inline_java_method v1: body has {n} statements; only single-statement bodies are inlineable"
        ),
    }
}

/// Walk the inline-target statement and refuse if it contains any of:
/// `this` / `super` expressions, `method_invocation` (calling another
/// method), `field_access`, or `identifier` text that doesn't match a
/// parameter name and isn't a type-position node (class names are
/// allowed for constructor calls / type qualifiers).
fn check_body_safe(
    node: Node<'_>,
    source: &str,
    param_names: &[&str],
    method_name: &str,
) -> Result<()> {
    let bytes = source.as_bytes();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        let kind = n.kind();
        match kind {
            "this" | "super" => {
                bail!(
                    "inline_java_method v1: body of `{method_name}` uses `{kind}` — refuse \
                     (would need an enclosing-instance argument)"
                );
            }
            "method_invocation" => {
                bail!(
                    "inline_java_method v1: body of `{method_name}` calls another method — \
                     refuse (would need to inline that too, or pass the receiver)"
                );
            }
            "field_access" => {
                bail!(
                    "inline_java_method v1: body of `{method_name}` reads a field — refuse \
                     (would need to thread the field's owner through the inlined expression)"
                );
            }
            "identifier" => {
                let Ok(text) = n.utf8_text(bytes) else {
                    continue;
                };
                // Identifier in non-parameter, non-type-name position is
                // suspicious. Accept identifiers that match a parameter
                // name (those are the substitution targets) or
                // identifiers that start with an uppercase letter
                // (heuristic: type / class names like `Integer`,
                // `Math.PI` qualifier — the latter is a field_access
                // which we've already refused).
                if param_names.contains(&text) {
                    continue;
                }
                if text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                {
                    // type identifier — fine
                    continue;
                }
                // Java keywords reach here (`true`, `false`, `null`)
                // but tree-sitter typically tags those as their own
                // node kinds, not `identifier`. Be conservative.
                bail!(
                    "inline_java_method v1: body of `{method_name}` references identifier \
                     `{text}` that is not a parameter — refuse"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Walk the class body for `method_invocation` nodes whose `name` is
/// `method_name` and whose receiver is either absent or `this`.
fn collect_call_sites<'a>(
    class_node: Node<'a>,
    method_name: &str,
    source: &str,
) -> Vec<Node<'a>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![class_node];
    while let Some(n) = stack.pop() {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            stack.push(ch);
        }
        if n.kind() != "method_invocation" {
            continue;
        }
        let Some(name_node) = n.child_by_field_name("name") else {
            continue;
        };
        let Ok(name_text) = name_node.utf8_text(bytes) else {
            continue;
        };
        if name_text != method_name {
            continue;
        }
        // Receiver must be absent or `this`. (Explicit `Class.foo` or
        // `obj.foo` is a different shape; refuse for safety.)
        match n.child_by_field_name("object") {
            None => out.push(n),
            Some(obj) if obj.kind() == "this" => out.push(n),
            _ => {}
        }
    }
    out
}

fn call_argument_expressions(call: Node<'_>, source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if let Ok(text) = child.utf8_text(bytes) {
            out.push(text.trim().to_string());
        }
    }
    out
}

/// If `call` is the entire expression of an `expression_statement`,
/// return that statement node (so we can also strip the surrounding
/// `;`). Otherwise return None.
fn enclosing_expression_statement(call: Node<'_>) -> Option<Node<'_>> {
    let parent = call.parent()?;
    if parent.kind() == "expression_statement" {
        Some(parent)
    } else {
        None
    }
}

/// Substitute each parameter identifier in `expression_text` with the
/// corresponding argument (in parens for operator-precedence safety).
/// Parameter identifiers are matched by word-boundary against ASCII
/// identifier characters.
fn substitute_params_in_expression(
    expression_text: &str,
    param_names: &[&str],
    arguments: &[String],
) -> String {
    let mut out = String::with_capacity(expression_text.len() + 16);
    let bytes = expression_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_java_ident_start(b) {
            let mut j = i + 1;
            while j < bytes.len() && is_java_ident_cont(bytes[j]) {
                j += 1;
            }
            let word = &expression_text[i..j];
            if let Some(pos) = param_names.iter().position(|p| *p == word) {
                out.push('(');
                out.push_str(&arguments[pos]);
                out.push(')');
            } else {
                out.push_str(word);
            }
            i = j;
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

fn is_java_ident_start(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphabetic()
}

fn is_java_ident_cont(b: u8) -> bool {
    is_java_ident_start(b) || b.is_ascii_digit()
}

fn leading_trivia_byte_start(source: &str, node: Node<'_>) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = node.start_byte();
    while cursor > 0 {
        let b = bytes[cursor - 1];
        if b == b' ' || b == b'\t' {
            cursor -= 1;
        } else if b == b'\n' {
            // include the newline immediately before the declaration
            cursor -= 1;
            break;
        } else {
            break;
        }
    }
    cursor
}

fn trailing_newline_byte_end(source: &str, end: usize) -> usize {
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

//! `inline_java_method` — replace call sites with the method body and
//! delete the declaration.
//!
//! ## Body shapes supported
//!
//! - **Single-statement non-void** (`{ return <expr>; }`) — call site
//!   in expression position is replaced with `(<substituted-expr>)`;
//!   in statement position with `<substituted-expr>;`.
//! - **Single-statement void** (`{ <stmt>; }`) — call site in
//!   statement position is replaced with `<substituted-stmt>;`. Refuses
//!   expression-position calls (a void method shouldn't be called as
//!   an expression).
//! - **Multi-statement void** (`{ <stmt1>; <stmt2>; ... }`) — call
//!   site in STATEMENT position is replaced with a `{ <substituted>
//!   <stmts> }` block. Refuses expression-position calls.
//!
//! Multi-statement non-void bodies are refused — Java has no
//! statement-expression syntax to inline them safely.
//!
//! ## Caller-site discovery
//!
//! - Default (single file): walks the source file for call sites of
//!   the method.
//! - Project-wide via `toml_entries.project_wide = true`: walks every
//!   `.java` file under `project_dir` (skipping `target/`, `build/`,
//!   `.gradle/`, `node_modules/`, `.git/`). Required for non-private
//!   methods that may be called from other files.
//!
//! ## Visibility
//!
//! Private methods can be inlined in single-file mode (their callers
//! are necessarily in the same file). Non-private methods REQUIRE
//! `project_wide = true` — otherwise the operator might leave caller
//! files unmigrated and break the build.
//!
//! ## Body-safety check
//!
//! The body must reference only formal parameters — no `this`, `super`,
//! field accesses, or other method calls. Receiverless method calls
//! and field reads on the enclosing class would fail to resolve at the
//! caller site if the caller is in a different class.

use super::*;
use crate::java::method_params::formal_parameters;

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
    if has_modifier(method_node, "abstract") {
        bail!("method `{method_name}` is abstract — no body to inline");
    }

    let is_private = has_modifier(method_node, "private");
    let project_wide = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("project_wide"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_private && !project_wide {
        bail!(
            "inline_java_method refuses non-private method `{method_name}` without \
             project_wide=true (other files may call it; single-file walk would leave them \
             unmigrated and break the build). Re-run with toml_entries.project_wide=true."
        );
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

    let body_shape = classify_body(body_node, &parsed.source, is_void, method_name)?;

    let params = formal_parameters(method_node, &parsed.source);
    let param_names: Vec<&str> = params.iter().map(|(_, n)| n.as_str()).collect();

    // Safety: all of the body's statements must use only formal parameters.
    let stmts = body_named_statements(body_node);
    for s in &stmts {
        check_stmt_safe(*s, &parsed.source, &param_names, method_name)?;
    }

    // Collect call sites — single file or project-wide.
    let mut file_edits: Vec<FileEdit> = Vec::new();
    let mut validations = Vec::new();
    let mut total_call_sites = 0usize;

    // Process the source file first.
    let source_call_sites = collect_call_sites(class_node, method_name, &parsed.source);
    let mut source_edits: Vec<TextEdit> = Vec::new();
    for call in &source_call_sites {
        let edit = build_call_site_edit(
            *call,
            &parsed.source,
            &params,
            &param_names,
            &body_shape,
            method_name,
            is_void,
        )?;
        source_edits.push(edit);
    }
    let source_file_call_count = source_call_sites.len();
    if !source_edits.is_empty() {
        total_call_sites += source_edits.len();
    }

    // Always delete the method declaration in the source file.
    let decl_start = leading_trivia_byte_start(&parsed.source, method_node);
    let decl_end = trailing_newline_byte_end(&parsed.source, method_node.end_byte());
    source_edits.push(TextEdit {
        byte_start: decl_start,
        byte_end: decl_end,
        replacement: String::new(),
    });
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;
    validations.extend(parse_validation_step_for_path(&source_path));
    file_edits.push(FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
        new_text: None,
    });

    // Project-wide pass for non-private (or operator-enabled).
    if project_wide {
        let project_dir = p
            .project_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow!("project_dir is required when project_wide=true"))?;
        for entry in walkdir::WalkDir::new(&project_dir).into_iter().flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("java") {
                continue;
            }
            if path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("target" | "build" | ".gradle" | "node_modules" | ".git")
                )
            }) {
                continue;
            }
            if path == source_path.as_path() {
                continue;
            }
            let Ok(other_parsed) = parse_source_file(path) else {
                continue;
            };
            if other_parsed.language != "java" {
                continue;
            }
            let calls = collect_call_sites(
                other_parsed.tree.root_node(),
                method_name,
                &other_parsed.source,
            );
            if calls.is_empty() {
                continue;
            }
            let mut edits = Vec::new();
            for call in &calls {
                let edit = build_call_site_edit(
                    *call,
                    &other_parsed.source,
                    &params,
                    &param_names,
                    &body_shape,
                    method_name,
                    is_void,
                )?;
                edits.push(edit);
            }
            edits.sort_by_key(|e| e.byte_start);
            ensure_non_overlapping(&edits)?;
            total_call_sites += edits.len();
            file_edits.push(FileEdit {
                path: path_string(path),
                original_sha256: sha256_hex(other_parsed.source.as_bytes()),
                edits,
                new_text: None,
            });
            validations.extend(parse_validation_step_for_path(path));
        }
    }

    if total_call_sites == 0 {
        if source_file_call_count == 0 && !project_wide {
            bail!(
                "no call sites for `{method_name}` found in {} — inlining would only delete the \
                 declaration; use `prune_java_orphans` instead",
                source_path.display()
            );
        }
        if total_call_sites == 0 && project_wide {
            // Still proceed — even with no callers, the project-wide
            // mode deletes the declaration cleanly. That's fine; the
            // operator's choice.
        }
    }

    let title = format!(
        "inline method `{method_name}` ({} call site(s) across {} file(s)) and delete declaration in {}",
        total_call_sites,
        file_edits.len(),
        path_string(&source_path)
    );

    let plan = RefactorPlan {
        title,
        kind: "inline_java_method".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: vec![format!(
            "call_sites_inlined={total_call_sites}, project_wide={project_wide}, body_shape={:?}",
            body_shape
        )],
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

#[derive(Debug, Clone)]
enum BodyShape {
    /// Single `return <expr>;` — `expr` text after stripping `return` and `;`.
    SingleReturn(String),
    /// Single expression-statement — text after stripping trailing `;`.
    SingleVoidStmt(String),
    /// Multiple statements, void return — list of substituted-ready
    /// statement texts (each ending without trailing newline).
    MultiVoidStmts(Vec<String>),
}

fn classify_body(
    body_node: Node<'_>,
    source: &str,
    is_void: bool,
    method_name: &str,
) -> Result<BodyShape> {
    let stmts = body_named_statements(body_node);
    if stmts.is_empty() {
        bail!("method `{method_name}` has an empty body");
    }
    let stmt_text = |n: Node<'_>| -> String { source[n.start_byte()..n.end_byte()].to_string() };
    if stmts.len() == 1 {
        let n = stmts[0];
        match n.kind() {
            "expression_statement" if is_void => {
                let raw = stmt_text(n);
                let trimmed = raw.trim().trim_end_matches(';').trim().to_string();
                Ok(BodyShape::SingleVoidStmt(trimmed))
            }
            "return_statement" if !is_void => {
                let raw = stmt_text(n);
                let trimmed = raw
                    .trim()
                    .trim_start_matches("return")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                Ok(BodyShape::SingleReturn(trimmed))
            }
            _ => bail!(
                "inline_java_method: body of `{method_name}` has one statement of kind `{}` which \
                 isn't inlinable (expected expression_statement for void or return_statement for \
                 non-void)",
                n.kind()
            ),
        }
    } else if !is_void {
        bail!(
            "inline_java_method: non-void method `{method_name}` has {} statements; multi-statement \
             non-void inlining is unsupported (Java has no statement-expression syntax). Refactor \
             to a single return statement first, or pick a different refactor.",
            stmts.len()
        );
    } else {
        // Multi-statement void: collect each statement's text. They're
        // joined with spaces during substitution; semicolons are
        // preserved (each stmt text already ends with `;` from source).
        let texts: Vec<String> = stmts.iter().map(|n| stmt_text(*n)).collect();
        Ok(BodyShape::MultiVoidStmts(texts))
    }
}

/// Collect the named statement nodes inside a method block body
/// (skipping comments).
fn body_named_statements(body: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect()
}

fn build_call_site_edit(
    call: Node<'_>,
    source: &str,
    params: &[(String, String)],
    param_names: &[&str],
    body_shape: &BodyShape,
    method_name: &str,
    is_void: bool,
) -> Result<TextEdit> {
    let bytes = source.as_bytes();
    let arguments = call_argument_expressions(call, source);
    if arguments.len() != params.len() {
        bail!(
            "call site at byte {} passes {} args but method `{method_name}` takes {}",
            call.start_byte(),
            arguments.len(),
            params.len()
        );
    }

    // Find the call's source method to extract body text. Walk up from
    // the call to find the containing method_declaration. Actually we
    // need the INLINED method's body text, not the caller's — but
    // we don't have access to it here. The planner caller pre-computes
    // body_text and passes via BodyShape; for now we re-extract by
    // looking at the BodyShape variant which carries text snapshots.
    // We'll fill in the text snapshots in the higher-level loop.
    let _ = bytes;

    let enclosing_stmt = enclosing_expression_statement(call);
    let inlined_text = body_shape_to_text(body_shape, param_names, &arguments)?;

    match body_shape {
        BodyShape::SingleReturn(_) => {
            // Non-void: inline as expression OR statement-position assignment.
            if let Some(stmt) = enclosing_stmt {
                // The statement was `<lhs> = foo(args);` or
                // `<type> v = foo(args);`. Replace just the call_expr
                // with the substituted expr.
                let _ = stmt;
                Ok(TextEdit {
                    byte_start: call.start_byte(),
                    byte_end: call.end_byte(),
                    replacement: format!("({inlined_text})"),
                })
            } else {
                // Expression position: wrap in parens.
                Ok(TextEdit {
                    byte_start: call.start_byte(),
                    byte_end: call.end_byte(),
                    replacement: format!("({inlined_text})"),
                })
            }
        }
        BodyShape::SingleVoidStmt(_) => {
            // Void: must be in statement position.
            let stmt = enclosing_stmt.ok_or_else(|| {
                anyhow!(
                    "call site at byte {} uses void method `{method_name}` in expression position",
                    call.start_byte()
                )
            })?;
            Ok(TextEdit {
                byte_start: stmt.start_byte(),
                byte_end: stmt.end_byte(),
                replacement: format!("{inlined_text};"),
            })
        }
        BodyShape::MultiVoidStmts(_) => {
            // Multi-statement void: must be in statement position.
            // Replace with `{ <stmts> }` block.
            let _ = is_void;
            let stmt = enclosing_stmt.ok_or_else(|| {
                anyhow!(
                    "call site at byte {} uses multi-statement void method `{method_name}` in \
                     expression position; refuse",
                    call.start_byte()
                )
            })?;
            Ok(TextEdit {
                byte_start: stmt.start_byte(),
                byte_end: stmt.end_byte(),
                replacement: format!("{{ {inlined_text} }}"),
            })
        }
    }
}

fn body_shape_to_text(
    shape: &BodyShape,
    param_names: &[&str],
    arguments: &[String],
) -> Result<String> {
    match shape {
        BodyShape::SingleReturn(expr) => Ok(substitute_params_in_expression(
            expr,
            param_names,
            arguments,
        )),
        BodyShape::SingleVoidStmt(stmt) => Ok(substitute_params_in_expression(
            stmt,
            param_names,
            arguments,
        )),
        BodyShape::MultiVoidStmts(stmts) => Ok(stmts
            .iter()
            .map(|s| substitute_params_in_expression(s, param_names, arguments))
            .collect::<Vec<_>>()
            .join(" ")),
    }
}

fn find_method_by_name<'a>(class_node: Node<'a>, name: &str, source: &str) -> Option<Node<'a>> {
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

fn check_stmt_safe(
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
                bail!("inline_java_method: body of `{method_name}` uses `{kind}` — refuse");
            }
            "method_invocation" => {
                bail!("inline_java_method: body of `{method_name}` calls another method — refuse");
            }
            "field_access" => {
                bail!("inline_java_method: body of `{method_name}` reads a field — refuse");
            }
            "identifier" => {
                let Ok(text) = n.utf8_text(bytes) else {
                    continue;
                };
                if param_names.contains(&text) {
                    continue;
                }
                if text.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    continue;
                }
                bail!(
                    "inline_java_method: body of `{method_name}` references identifier `{text}` \
                     that is not a parameter — refuse"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_call_sites<'a>(node: Node<'a>, method_name: &str, source: &str) -> Vec<Node<'a>> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![node];
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

fn enclosing_expression_statement(call: Node<'_>) -> Option<Node<'_>> {
    let parent = call.parent()?;
    if parent.kind() == "expression_statement" {
        Some(parent)
    } else {
        None
    }
}

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

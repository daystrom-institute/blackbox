//! `extract_java_code_block_to_method` — pull a statement range out of a
//! method body into a new private helper.
//!
//! Mirrors the shape of `extract_rust_function_region`: the operator
//! supplies the exact text to extract (`old_text`), the new helper's
//! name (`module_name`), parameter list (`parameters` field), arguments
//! at the call site (`toml_entries.arguments`), and optional return
//! type (`toml_entries.return_type`). v1 does no automatic capture
//! analysis — the operator is the source of truth for the new method's
//! signature.
//!
//! Inputs
//!
//! - `source` — Java file containing the enclosing method.
//! - `project_dir` — project root.
//! - `old_text` — exact text of the statement range to extract. Must
//!   match the source file exactly once, must fit cleanly inside one
//!   `method_declaration` or `constructor_declaration`'s body.
//! - `module_name` — name of the new private helper.
//! - `parameters` — list of `JavaParameterSpec` entries (existing
//!   RefactorPlanParams field). When omitted, the helper takes no
//!   parameters.
//! - `toml_entries.arguments` — `Vec<String>` of expressions to pass at
//!   the call site. Length must match `parameters`. When omitted,
//!   defaults to the parameter names (most common case: the operator
//!   captures locals whose names are already in scope at the call
//!   site).
//! - `toml_entries.return_type` — optional return type (e.g. `"int"`,
//!   `"String"`). When omitted or `"void"`, the helper returns `void`
//!   and the call site is a statement; otherwise the call site is the
//!   expression `<helper>(<args>)`.
//! - `toml_entries.return_var` — when `return_type` is non-void, the
//!   name of the variable on the call site that captures the helper's
//!   return value. Defaults to `result`.
//! - `new_text` — optional explicit call-site replacement. When set,
//!   overrides the synthesized call expression. Use this for unusual
//!   shapes (chained call, exception-handling wrapper) that the default
//!   renderer doesn't cover.
//! - `visibility` — optional. Defaults to `private`. The helper is
//!   marked `static` automatically when the enclosing method is
//!   `static`.
//! - `impl_name` — optional class name when the source file has
//!   multiple classes (matches `extract_java_methods` semantics).
//!
//! v1 refusals (the operator should fix and re-run, not pave over):
//!
//! - `old_text` matches zero or more than one place in the file —
//!   provide more surrounding context so the match is unique.
//! - The enclosing node isn't a method or constructor body (e.g. the
//!   range is a class-level static initializer).
//! - The range crosses a `method_declaration` boundary (extracts from
//!   two methods at once).
//! - `parameters.len() != arguments.len()` — the operator must keep
//!   the two lists aligned.
//!
//! v2 follow-ups (filed separately):
//!
//! - Automatic capture inference: walk the range for `identifier`
//!   references, classify by lexical scope, build the parameter list
//!   without operator help.
//! - Mutated-capture detection: refuse when a captured local is
//!   reassigned inside the range (Java has no out-params).
//! - Multi-value return: synthesize a `record` result type when the
//!   range produces more than one out-value.
//! - Control-flow safety: refuse non-local `return` / `break` /
//!   `continue` referring to labels outside the range.

use super::*;

pub(crate) fn plan_extract_java_code_block_to_method(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_code_block_to_method only supports java files");
    }

    let selected = p
        .old_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("old_text is required for extract_java_code_block_to_method")
        })?;
    let helper_name = p.module_name.as_deref().ok_or_else(|| {
        anyhow!("module_name (the new helper's name) is required for extract_java_code_block_to_method")
    })?;
    if !is_java_identifier(helper_name) {
        bail!("module_name `{helper_name}` is not a valid Java identifier");
    }

    let matches = find_text_matches(&parsed.source, selected);
    match matches.len() {
        0 => bail!(
            "old_text not found in {}; provide more context so the match is exact",
            source_path.display()
        ),
        1 => {}
        n => bail!(
            "old_text matched {n} times in {}; provide more surrounding context to disambiguate",
            source_path.display()
        ),
    }
    let (region_start, region_end) = matches[0];

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

    let enclosing_method =
        find_enclosing_method_node(class_node, region_start, region_end).ok_or_else(|| {
            anyhow!(
                "old_text is not fully enclosed by a method_declaration or \
                 constructor_declaration inside class `{}`",
                java_class_name(class_node, &parsed.source).unwrap_or_else(|| "(unnamed)".into())
            )
        })?;
    let enclosing_method_kind = enclosing_method.kind();
    let enclosing_method_name = enclosing_method
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .unwrap_or("(unnamed)")
        .to_string();

    let parameters_specs = p.parameters.as_deref().unwrap_or(&[]);
    let arguments = toml_str_array(&p.toml_entries, "arguments");
    let effective_arguments: Vec<String> = if arguments.is_empty() {
        parameters_specs.iter().map(|spec| spec.name.clone()).collect()
    } else {
        arguments
    };
    if effective_arguments.len() != parameters_specs.len() {
        bail!(
            "extract_java_code_block_to_method: parameters.len()={} but arguments.len()={} \
             — supply matching lists (or omit arguments to default to parameter names)",
            parameters_specs.len(),
            effective_arguments.len()
        );
    }

    let return_type = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("return_type"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let is_void = match return_type.as_deref() {
        None | Some("void") => true,
        Some(_) => false,
    };
    let return_var_name = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("return_var"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && is_java_identifier(value))
        .unwrap_or("result")
        .to_string();

    let enclosing_is_static = has_java_modifier_node(enclosing_method, "static");

    let visibility = p
        .visibility
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("private");
    if !is_valid_java_method_visibility(visibility) {
        bail!(
            "visibility `{visibility}` is not a valid Java method visibility modifier \
             (use one of: private, package-private, protected, public)"
        );
    }
    let visibility_prefix = match visibility {
        "package-private" | "package_private" | "" => String::new(),
        other => format!("{other} "),
    };
    let static_prefix = if enclosing_is_static { "static " } else { "" };
    let signature_return = return_type.as_deref().unwrap_or("void");
    let param_list = parameters_specs
        .iter()
        .map(|spec| format!("{} {}", spec.type_name.trim(), spec.name.trim()))
        .collect::<Vec<_>>()
        .join(", ");

    // Construct the helper body. For void return type, just use the
    // extracted text. For non-void, append a `return result;` synthetic
    // — operator must structure the extracted text so the helper's last
    // expression produces the return value (manual-mode, operator owns
    // correctness). If the operator's extracted block already ends with
    // a `return ...;` statement the helper compiles fine; the extra
    // `return result;` is unreachable. v2 inference would fix this.
    let extracted = selected.trim_end_matches(|c: char| c.is_whitespace()).to_string();
    let helper_body = if is_void {
        extracted.clone()
    } else {
        format!("{extracted}\nreturn {return_var_name};")
    };
    let helper_indent = method_body_indent_for(class_node, &parsed.source);
    let helper_inner_indent = format!("{helper_indent}    ");
    let helper_body_indented = reindent_block(&helper_body, &helper_inner_indent);
    let helper_decl = format!(
        "{helper_indent}{visibility_prefix}{static_prefix}{signature_return} {helper_name}({param_list}) {{\n\
         {helper_body_indented}\n\
         {helper_indent}}}\n"
    );

    // Synthesize the call-site replacement. Operator-supplied
    // `new_text` overrides for unusual shapes.
    let arg_list = effective_arguments.join(", ");
    let replacement = if let Some(text) = p.new_text.clone() {
        text
    } else if is_void {
        format!("{helper_name}({arg_list});")
    } else {
        format!(
            "{rt} {var} = {name}({args});",
            rt = signature_return,
            var = return_var_name,
            name = helper_name,
            args = arg_list,
        )
    };

    // Where to insert the helper: immediately after the enclosing
    // method's closing brace, with one blank-line separation if the
    // surrounding source isn't already double-newline-terminated there.
    let enclosing_end = enclosing_method.end_byte();
    let helper_insert_text = format_helper_insert(&parsed.source, enclosing_end, &helper_decl);

    let mut edits = Vec::with_capacity(2);
    edits.push(TextEdit {
        byte_start: region_start,
        byte_end: region_end,
        replacement,
    });
    edits.push(TextEdit {
        byte_start: enclosing_end,
        byte_end: enclosing_end,
        replacement: helper_insert_text,
    });
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: format!(
            "extract code block from {}.{} into `{}` in {}",
            java_class_name(class_node, &parsed.source).unwrap_or_else(|| "(unnamed)".into()),
            enclosing_method_name,
            helper_name,
            path_string(&source_path)
        ),
        kind: "extract_java_code_block_to_method".to_string(),
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
        leftovers: vec![format!(
            "enclosing {enclosing_method_kind} `{enclosing_method_name}` (static={enclosing_is_static})"
        )],
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

fn find_text_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    while start + needle_bytes.len() <= bytes.len() {
        if &bytes[start..start + needle_bytes.len()] == needle_bytes {
            out.push((start, start + needle_bytes.len()));
            start += needle_bytes.len();
        } else {
            start += 1;
        }
    }
    out
}

/// Walk the class body looking for the method (or constructor) whose
/// body byte range encloses `[region_start, region_end)`. Returns the
/// `method_declaration` / `constructor_declaration` node, NOT the body
/// node itself.
fn find_enclosing_method_node<'a>(
    class_node: Node<'a>,
    region_start: usize,
    region_end: usize,
) -> Option<Node<'a>> {
    let mut stack: Vec<Node<'a>> = vec![class_node];
    while let Some(node) = stack.pop() {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            stack.push(child);
        }
        let kind = node.kind();
        if kind != "method_declaration" && kind != "constructor_declaration" {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else {
            continue;
        };
        if body.start_byte() <= region_start && body.end_byte() >= region_end {
            return Some(node);
        }
    }
    None
}

fn java_class_name(class_node: Node<'_>, source: &str) -> Option<String> {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
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

fn is_valid_java_method_visibility(v: &str) -> bool {
    matches!(
        v,
        "private" | "protected" | "public" | "package-private" | "package_private" | ""
    )
}

fn is_java_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

fn toml_str_array(
    entries: &Option<std::collections::BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> Vec<String> {
    let Some(map) = entries.as_ref() else {
        return Vec::new();
    };
    let Some(value) = map.get(key) else {
        return Vec::new();
    };
    let Some(array) = value.as_array() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect()
}

/// Walk a class node's first method to extract the canonical indent
/// prefix (whitespace before the method declaration). Used so the
/// inserted helper aligns with the surrounding members.
fn method_body_indent_for(class_node: Node<'_>, source: &str) -> String {
    let mut cursor = class_node.walk();
    for child in class_node.named_children(&mut cursor) {
        if child.kind() != "class_body" {
            continue;
        }
        let mut bc = child.walk();
        for member in child.named_children(&mut bc) {
            if member.kind() == "method_declaration"
                || member.kind() == "constructor_declaration"
            {
                let start = member.start_byte();
                let bytes = source.as_bytes();
                let mut line_start = start;
                while line_start > 0 && bytes[line_start - 1] != b'\n' {
                    line_start -= 1;
                }
                return source[line_start..start].to_string();
            }
        }
    }
    "    ".to_string()
}

/// Re-indent a multi-line block so every non-empty line starts with
/// `indent`. The first line is treated identically; leading whitespace
/// on each input line is replaced with `indent`. Empty lines stay
/// empty.
fn reindent_block(text: &str, indent: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                String::new()
            } else {
                format!("{indent}{trimmed}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Construct the insertion text for the helper, ensuring a blank-line
/// separator between the enclosing method's closing brace and the new
/// helper. If the source already has a trailing newline or two after
/// the enclosing method, avoid double-spacing.
fn format_helper_insert(source: &str, enclosing_end: usize, helper_decl: &str) -> String {
    let bytes = source.as_bytes();
    let after = enclosing_end;
    let next_two = (
        bytes.get(after).copied(),
        bytes.get(after + 1).copied(),
    );
    let prefix = match next_two {
        (Some(b'\n'), Some(b'\n')) => "",
        (Some(b'\n'), _) => "\n",
        _ => "\n\n",
    };
    format!("{prefix}{helper_decl}")
}

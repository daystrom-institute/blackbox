//! `extract_java_code_block_to_method` — pull a statement range out of a
//! method body into a new private helper.
//!
//! The planner builds a lexical scope tree for the enclosing method
//! (via `super::scope`) and analyzes the selected byte range to
//! automatically compute:
//!
//! - **Parameter list**: variables read inside the range whose
//!   declarations are outside the range. Each becomes a parameter of
//!   the new helper with its declared type.
//! - **Arguments**: at the call site, the captures pass through with
//!   their existing names.
//! - **Return value**: a variable declared inside the range and used
//!   at a later byte position in the enclosing method. The helper
//!   returns it; the call site captures it into a variable of the
//!   matching type. Zero such variables → helper returns `void`. With
//!   `toml_entries.result_record=true`, multiple visible, explicitly
//!   typed live-outs can be bundled into a generated nested record.
//!
//! ## Refusals (the extract is unsafe / impossible at the requested range)
//!
//! - `error.mutated_capture(name)` — a captured variable is reassigned
//!   inside the range. Java has no out-params, so the post-call value
//!   wouldn't propagate back. Restructure the algorithm so the mutation
//!   happens at the call site, or extract a smaller range that doesn't
//!   include the reassignment.
//! - `error.multi_return_needs_record` — more than one variable
//!   declared inside the range is used after. By default this still
//!   refuses. Pass `toml_entries.result_record=true` to synthesize a
//!   nested record result bundle when each live-out is visible at the
//!   helper return site and has an explicit Java type.
//! - `error.non_local_control_flow(kind)` — a `return` / `break` /
//!   `continue` inside the range targets a method or loop outside the
//!   range. The extract would change control-flow semantics. Either
//!   widen the range to include the target, or refactor the early-exit
//!   into a single return at the end of the range.
//! - `old_text` doesn't match exactly once.
//! - The matched range isn't inside any method or constructor body.
//!
//! ## Operator overrides
//!
//! - `parameters` (`Vec<JavaParameterSpec>`) — when set, REPLACES the
//!   inferred parameter list. Use this when the operator wants
//!   different param names (e.g. `total` instead of inferred `sum`) or
//!   different types (e.g. an interface instead of a concrete class).
//!   When override is used, `toml_entries.arguments` is required and
//!   must align in length.
//! - `new_text` — explicit call-site replacement, overrides the
//!   synthesized call expression.
//! - `visibility` — `private` / `protected` / `public` /
//!   `package-private`. Default `private`.
//! - `impl_name` — enclosing class name when source has multiple
//!   classes.

use super::scope::{ScopeTree, analyze_range};
use super::*;

#[derive(Debug, Clone)]
struct ResultRecordComponent {
    type_text: String,
    name: String,
}

#[derive(Debug, Clone)]
struct ResultRecordPlan {
    type_name: String,
    var_name: String,
    components: Vec<ResultRecordComponent>,
}

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
        .ok_or_else(|| anyhow!("old_text is required for extract_java_code_block_to_method"))?;
    let helper_name = p.module_name.as_deref().ok_or_else(|| {
        anyhow!(
            "module_name (the new helper's name) is required for extract_java_code_block_to_method"
        )
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

    let enclosing_method = find_enclosing_method_node(class_node, region_start, region_end)
        .ok_or_else(|| {
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

    // -----------------------------------------------------------------
    // Scope analysis — the load-bearing inference.
    // -----------------------------------------------------------------

    let scope_tree = ScopeTree::build_from_method(enclosing_method, &parsed.source);
    let analysis = analyze_range(
        &scope_tree,
        enclosing_method,
        region_start,
        region_end,
        &parsed.source,
    );

    // Refuse: mutated capture.
    if let Some(bad) = analysis.captures.iter().find(|c| c.mutated) {
        bail!(
            "error.mutated_capture({}): captured variable `{}` is reassigned inside the range. \
             Java has no out-parameters; extracting would silently drop the post-call value. \
             Restructure the algorithm so the reassignment happens at the call site, or pick a \
             smaller range.",
            bad.name,
            bad.name
        );
    }

    let result_record_requested = toml_bool(&p.toml_entries, "result_record");

    // Refuse: multi-return unless the caller explicitly requested the
    // generated result-record path.
    if analysis.inner_decls_used_after.len() > 1 && !result_record_requested {
        let names: Vec<&str> = analysis
            .inner_decls_used_after
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        bail!(
            "error.multi_return_needs_record: the range declares {} variables that are read after \
             ({}). Java methods return a single value; declare a `record` type for the bundle \
             yourself, then re-run with a smaller range that produces a single record value.",
            names.len(),
            names.join(", ")
        );
    }

    // Refuse: non-local control flow.
    if let Some(bad) = analysis.non_local_control_flow.first() {
        bail!(
            "error.non_local_control_flow({}): the range contains a `{}` that targets a method \
             or loop outside the selection. Extracting would change control-flow semantics. \
             Either widen the range to include the target, or refactor the early-exit into a \
             single return at the end of the range.",
            bad.kind,
            bad.kind.trim_end_matches("_statement")
        );
    }

    // -----------------------------------------------------------------
    // Parameter/argument list — infer unless operator overrides.
    // -----------------------------------------------------------------

    let (inferred_params, inferred_args): (Vec<(String, String)>, Vec<String>) = {
        let params = analysis
            .captures
            .iter()
            .map(|c| (c.type_text.clone(), c.name.clone()))
            .collect::<Vec<_>>();
        let args = analysis.captures.iter().map(|c| c.name.clone()).collect();
        (params, args)
    };

    let (effective_params, effective_args): (Vec<(String, String)>, Vec<String>) =
        match p.parameters.as_deref() {
            Some(specs) if !specs.is_empty() => {
                let supplied_args = toml_str_array(&p.toml_entries, "arguments");
                let args = if supplied_args.is_empty() {
                    specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
                } else {
                    supplied_args
                };
                if args.len() != specs.len() {
                    bail!(
                        "operator-supplied parameters.len()={} but arguments.len()={} — must match",
                        specs.len(),
                        args.len()
                    );
                }
                let params = specs
                    .iter()
                    .map(|s| (s.type_name.clone(), s.name.clone()))
                    .collect();
                (params, args)
            }
            _ => (inferred_params, inferred_args),
        };
    for (type_text, name) in &effective_params {
        if is_inferred_java_type(type_text) {
            bail!(
                "error.inferred_capture_parameter_type({}): captured helper parameter `{}` has \
                 inferred type `{}`; Java helper method parameters require explicit types. \
                 Resolve the declaration type and pass operator-supplied parameters/arguments.",
                name,
                name,
                type_text.trim()
            );
        }
    }

    // -----------------------------------------------------------------
    // Return type — infer from inner_decls_used_after (single var) or
    // operator-supplied toml_entries.return_type override.
    // -----------------------------------------------------------------

    let inferred_return: Option<(String, String)> = analysis
        .inner_decls_used_after
        .first()
        .map(|(name, ty)| (ty.clone(), name.clone()));

    let operator_return_type = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("return_type"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let operator_return_var = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("return_var"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && is_java_identifier(value))
        .map(str::to_string);

    let result_record = if analysis.inner_decls_used_after.len() > 1 {
        if operator_return_type.is_some() {
            bail!(
                "error.result_record_return_type_conflict: result_record=true cannot be combined \
                 with return_type; use result_record_name to choose the generated record type"
            );
        }
        Some(build_result_record_plan(
            p,
            helper_name,
            &scope_tree,
            &analysis,
            region_start,
            region_end,
            &parsed.source,
        )?)
    } else {
        None
    };

    let (return_type, return_var): (String, Option<String>) = match (
        result_record.as_ref(),
        operator_return_type.as_deref(),
        &inferred_return,
    ) {
        (Some(record), _, _) => (record.type_name.clone(), Some(record.var_name.clone())),
        (None, Some("void"), _) => ("void".to_string(), None),
        (None, Some(t), _) => (
            t.to_string(),
            operator_return_var.or_else(|| inferred_return.as_ref().map(|(_, n)| n.clone())),
        ),
        (None, None, Some((ty, name))) => (ty.clone(), Some(name.clone())),
        (None, None, None) => ("void".to_string(), None),
    };
    let is_void = return_type == "void";

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
    let param_list = effective_params
        .iter()
        .map(|(ty, name)| format!("{} {}", ty.trim(), name.trim()))
        .collect::<Vec<_>>()
        .join(", ");

    // Helper body. For non-void return type, the helper must end with
    // `return <return_var>;`. Since `return_var` is the inferred (or
    // operator-confirmed) variable declared INSIDE the range, the
    // extracted text already contains its declaration; the return is
    // synthesized after.
    let helper_indent = method_body_indent_for(class_node, &parsed.source);
    let helper_inner_indent = format!("{helper_indent}    ");
    let extracted = selected
        .trim_end_matches(|c: char| c.is_whitespace())
        .to_string();
    let mut helper_body_indented = reindent_block(&extracted, &helper_inner_indent);
    if let Some(record) = &result_record {
        if !helper_body_indented.is_empty() {
            helper_body_indented.push('\n');
        }
        helper_body_indented.push_str(&helper_inner_indent);
        helper_body_indented.push_str("return new ");
        helper_body_indented.push_str(&record.type_name);
        helper_body_indented.push('(');
        helper_body_indented.push_str(
            &record
                .components
                .iter()
                .map(|component| component.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        helper_body_indented.push_str(");");
    } else if !is_void {
        let ret_name = return_var.as_deref().unwrap_or("result");
        if !helper_body_indented.is_empty() {
            helper_body_indented.push('\n');
        }
        helper_body_indented.push_str(&helper_inner_indent);
        helper_body_indented.push_str("return ");
        helper_body_indented.push_str(ret_name);
        helper_body_indented.push(';');
    }
    let mut helper_decl = format!(
        "{helper_indent}{visibility_prefix}{static_prefix}{return_type} {helper_name}({param_list}) {{\n\
         {helper_body_indented}\n\
         {helper_indent}}}\n"
    );
    if let Some(record) = &result_record {
        helper_decl = format!(
            "{}\n\n{}",
            render_result_record_decl(record, &helper_indent),
            helper_decl
        );
    }

    // Call site.
    let arg_list = effective_args.join(", ");
    let call_site_indent = call_site_replacement_indent(&parsed.source, region_start);
    let call_site_line_indent = line_indent_at(&parsed.source, region_start);
    let replacement = if let Some(text) = p.new_text.clone() {
        text
    } else if let Some(record) = &result_record {
        let var = return_var.as_deref().unwrap_or("result");
        let mut text =
            format!("{call_site_indent}{return_type} {var} = {helper_name}({arg_list});");
        for component in &record.components {
            text.push('\n');
            text.push_str(&call_site_line_indent);
            text.push_str(component.type_text.trim());
            text.push(' ');
            text.push_str(&component.name);
            text.push_str(" = ");
            text.push_str(var);
            text.push('.');
            text.push_str(&component.name);
            text.push_str("();");
        }
        text
    } else if is_void {
        format!("{call_site_indent}{helper_name}({arg_list});")
    } else {
        // The call site captures the helper's return into a local of
        // the matching type, named to match the inner declaration we
        // hoisted (so post-range uses still bind correctly).
        let var = return_var.as_deref().unwrap_or("result");
        format!("{call_site_indent}{return_type} {var} = {helper_name}({arg_list});")
    };

    let enclosing_end = enclosing_method.end_byte();
    let (helper_insert_end, helper_insert_text) =
        format_helper_insert(&parsed.source, enclosing_end, &helper_decl);

    let mut edits = vec![
        TextEdit {
            byte_start: region_start,
            byte_end: region_end,
            replacement,
        },
        TextEdit {
            byte_start: enclosing_end,
            byte_end: helper_insert_end,
            replacement: helper_insert_text,
        },
    ];
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let mut leftovers = vec![format!(
        "enclosing {enclosing_method_kind} `{enclosing_method_name}` (static={enclosing_is_static})"
    )];
    if !analysis.enclosing_class_refs.is_empty() {
        leftovers.push(format!(
            "enclosing_class_refs={:?} (resolved via `this` at the helper site; safe)",
            analysis.enclosing_class_refs
        ));
    }
    if analysis.this_super_refs > 0 {
        leftovers.push(format!(
            "this_super_refs={} (preserved verbatim; helper is on the same class)",
            analysis.this_super_refs
        ));
    }
    if let Some(record) = &result_record {
        leftovers.push(format!(
            "result_record={} components=[{}]",
            record.type_name,
            record
                .components
                .iter()
                .map(|component| format!("{} {}", component.type_text, component.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let plan = RefactorPlan {
        title: format!(
            "extract code block from {}.{} into `{}` ({} param(s){}) in {}",
            java_class_name(class_node, &parsed.source).unwrap_or_else(|| "(unnamed)".into()),
            enclosing_method_name,
            helper_name,
            effective_params.len(),
            if is_void {
                String::new()
            } else {
                format!(", returns {return_type}")
            },
            path_string(&source_path)
        ),
        kind: "extract_java_code_block_to_method".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
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
        operator_opt_outs_used: Vec::new(),
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

fn toml_bool(
    entries: &Option<std::collections::BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> bool {
    entries
        .as_ref()
        .and_then(|map| map.get(key))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn toml_str(
    entries: &Option<std::collections::BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    entries
        .as_ref()
        .and_then(|map| map.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

fn build_result_record_plan(
    p: &RefactorPlanParams,
    helper_name: &str,
    scope_tree: &ScopeTree,
    analysis: &super::scope::RangeAnalysis,
    region_start: usize,
    region_end: usize,
    source: &str,
) -> Result<ResultRecordPlan> {
    let type_name = toml_str(&p.toml_entries, "result_record_name")
        .unwrap_or_else(|| default_result_record_name(helper_name));
    if !is_java_identifier(&type_name) {
        bail!("result_record_name `{type_name}` is not a valid Java identifier");
    }
    if source.contains(&format!("record {type_name}("))
        || source.contains(&format!("class {type_name} "))
        || source.contains(&format!("class {type_name}{{"))
    {
        bail!("result_record_name `{type_name}` conflicts with an existing nested type");
    }
    let var_name = toml_str(&p.toml_entries, "result_record_var")
        .unwrap_or_else(|| format!("{}Result", lower_camel_identifier(helper_name)));
    if !is_java_identifier(&var_name) {
        bail!("result_record_var `{var_name}` is not a valid Java identifier");
    }

    let mut components = Vec::new();
    for decl in live_out_decls_in_declaration_order(
        scope_tree,
        &analysis.inner_decls_used_after,
        region_start,
        region_end,
    ) {
        if decl.scope_end <= region_end {
            bail!(
                "error.result_record_live_out_not_visible({}): live-out `{}` is not visible at \
                 the helper return site; widen the range to include the declaring scope or pick a \
                 top-level statement block",
                decl.name,
                decl.name
            );
        }
        let type_text = decl.type_text.trim();
        if is_inferred_java_type(type_text) {
            bail!(
                "error.result_record_inferred_type({}): live-out `{}` has inferred type `{}`; \
                 result-record generation requires explicit component types",
                decl.name,
                decl.name,
                type_text
            );
        }
        components.push(ResultRecordComponent {
            type_text: type_text.to_string(),
            name: decl.name.clone(),
        });
    }
    if components.len() != analysis.inner_decls_used_after.len() {
        let names = analysis
            .inner_decls_used_after
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "error.result_record_live_out_resolution: could not resolve live-out declarations for ({names})"
        );
    }

    Ok(ResultRecordPlan {
        type_name,
        var_name,
        components,
    })
}

fn is_inferred_java_type(type_text: &str) -> bool {
    let trimmed = type_text.trim();
    trimmed.is_empty() || trimmed == "var" || trimmed == "<inferred>"
}

fn live_out_decls_in_declaration_order<'a>(
    scope_tree: &'a ScopeTree,
    live_outs: &[(String, String)],
    region_start: usize,
    region_end: usize,
) -> Vec<&'a super::scope::Declaration> {
    scope_tree
        .declarations()
        .iter()
        .filter(|decl| {
            decl.name_byte_start >= region_start
                && decl.name_byte_end <= region_end
                && live_outs.iter().any(|(name, _)| name == &decl.name)
        })
        .collect()
}

fn default_result_record_name(helper_name: &str) -> String {
    let mut chars = helper_name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {
            format!(
                "{}{}Result",
                first.to_ascii_uppercase(),
                chars.collect::<String>()
            )
        }
        Some(first) => format!("{}{}Result", first, chars.collect::<String>()),
        None => "ExtractedResult".to_string(),
    }
}

fn lower_camel_identifier(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => {
            format!(
                "{}{}",
                first.to_ascii_lowercase(),
                chars.collect::<String>()
            )
        }
        Some(_) => name.to_string(),
        None => "result".to_string(),
    }
}

fn render_result_record_decl(record: &ResultRecordPlan, helper_indent: &str) -> String {
    let components = record
        .components
        .iter()
        .map(|component| format!("{} {}", component.type_text.trim(), component.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{helper_indent}private record {}({}) {{}}",
        record.type_name, components
    )
}

fn method_body_indent_for(class_node: Node<'_>, source: &str) -> String {
    let mut cursor = class_node.walk();
    for child in class_node.named_children(&mut cursor) {
        if child.kind() != "class_body" {
            continue;
        }
        let mut bc = child.walk();
        for member in child.named_children(&mut bc) {
            if member.kind() == "method_declaration" || member.kind() == "constructor_declaration" {
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

fn line_indent_at(source: &str, byte: usize) -> String {
    let bytes = source.as_bytes();
    let mut line_start = byte.min(bytes.len());
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut cursor = line_start;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
        cursor += 1;
    }
    source[line_start..cursor].to_string()
}

fn call_site_replacement_indent(source: &str, byte: usize) -> String {
    let bytes = source.as_bytes();
    let mut line_start = byte.min(bytes.len());
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    if line_start == byte {
        return line_indent_at(source, byte);
    }
    // The edit starts after existing line-leading whitespace, so that
    // whitespace remains in the file and must not be duplicated.
    String::new()
}

fn reindent_block(text: &str, indent: &str) -> String {
    let mut line_indents = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.bytes().take_while(|b| *b == b' ').count())
        .collect::<Vec<_>>();
    let min_indent = if line_indents.first().copied() == Some(0)
        && line_indents.iter().skip(1).all(|count| *count > 0)
    {
        line_indents.iter().skip(1).copied().min().unwrap_or(0)
    } else {
        line_indents.drain(..).min().unwrap_or(0)
    };

    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                let leading_spaces = line.bytes().take_while(|b| *b == b' ').count();
                let strip = min_indent.min(leading_spaces);
                format!("{indent}{}", &line[strip..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_helper_insert(source: &str, enclosing_end: usize, helper_decl: &str) -> (usize, String) {
    let bytes = source.as_bytes();
    let mut whitespace_end = enclosing_end;
    while whitespace_end < bytes.len()
        && matches!(bytes[whitespace_end], b' ' | b'\t' | b'\r' | b'\n')
    {
        whitespace_end += 1;
    }

    let separator_end = source[enclosing_end..whitespace_end]
        .rfind('\n')
        .map(|idx| enclosing_end + idx + 1)
        .unwrap_or(enclosing_end);
    let next_non_ws = bytes.get(whitespace_end).copied();
    let suffix = match next_non_ws {
        Some(b'}') | None => "",
        _ => "\n",
    };
    (
        separator_end,
        format!("\n\n{}{}\n", helper_decl.trim_end_matches('\n'), suffix),
    )
}

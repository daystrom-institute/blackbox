//! RX-S2b — `update_rust_callers` plan kind.
//!
//! Conservatively rewrites `self.field` and `self.method(args)` accesses
//! through a delegate field. Only Copy-whitelisted rvalue reads and
//! unambiguous method calls are rewritten. Everything else goes to
//! `unrewriteable_accessors`. `semantic_status: IndexedHints`.

use super::*;
use super::rust_deep;
use super::rust_warning_markers;
use std::collections::HashMap;
use std::collections::HashSet;

// ── response wrapper ──────────────────────────────────────────────────────────

/// RefactorPlan extended with caller-rewrite analysis fields.
#[derive(Debug, Serialize)]
struct PlanWithCallerExtra {
    #[serde(flatten)]
    plan: RefactorPlan,
    /// Access sites that could not be safely rewritten automatically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unrewriteable_accessors: Vec<RemainingFieldAccessor>,
    /// Field-read rewrite sites inside `&self` functions where the generated
    /// accessor call may require `&mut self` on the delegate. Requires
    /// post-apply compiler verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    borrow_promotions: Vec<FieldAccessSite>,
    /// Candidate rewrite sites nested inside the byte range of an outer rewrite;
    /// excluded from FileEdits to maintain the non-overlapping invariant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    overlapping_rewrite_sites: Vec<FieldAccessSite>,
}

// ── rewrite candidate ─────────────────────────────────────────────────────────

struct Candidate {
    byte_start: usize,
    byte_end: usize,
    replacement: String,
    name: String,
    /// "method_call" | "field_read" | "method_ref"
    kind: &'static str,
    line: usize,
    column: usize,
    context: String,
    /// True when the site is inside a `&self` receiver function.
    in_self_receiver: bool,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn plan_update_callers(p: &RefactorPlanParams) -> anyhow::Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;

    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required"))?;

    let item_names: HashSet<String> = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("item_names (moved fields/methods) is required"))?
        .iter()
        .cloned()
        .collect();

    let struct_name = p.impl_name.as_deref().or(p.module_name.as_deref());

    let parsed = parse_rust_file(&source_path)?;
    let source_bytes = parsed.source.as_bytes();

    // ── collect field types for Copy whitelist check ──────────────────────────
    let mut field_types: HashMap<String, String> = HashMap::new();
    if let Some(sname) = struct_name {
        collect_struct_field_types_from_parsed(&parsed, sname, source_bytes, &mut field_types);
    }
    // Also check target file (delegate type) when source no longer has the field.
    if let Some(target_str) = p.target.as_deref() {
        if let Ok(tgt_path) = resolve_path(p.project_dir.as_deref(), target_str) {
            if let Ok(parsed_tgt) = parse_rust_file(&tgt_path) {
                let tgt_bytes = parsed_tgt.source.as_bytes();
                if let Some(dtn) = p.delegate_type.as_deref() {
                    collect_struct_field_types_from_parsed(
                        &parsed_tgt,
                        dtn,
                        tgt_bytes,
                        &mut field_types,
                    );
                }
            }
        }
    }

    // ── walk AST for rewrite candidates ──────────────────────────────────────
    let root = parsed.tree.root_node();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut unrewriteable: HashMap<String, Vec<FieldAccessSite>> = HashMap::new();

    walk_for_rewrites(
        &parsed.source,
        source_bytes,
        root,
        &item_names,
        &field_types,
        delegate_field,
        &mut candidates,
        &mut unrewriteable,
    );

    // ── overlap detection + edit assembly ────────────────────────────────────
    // Sort candidates by byte_start so containment check is a forward scan.
    candidates.sort_by_key(|c| c.byte_start);

    let mut edits: Vec<TextEdit> = Vec::new();
    let mut overlapping: Vec<FieldAccessSite> = Vec::new();
    let mut borrow_promotions: Vec<FieldAccessSite> = Vec::new();
    // Each entry is the [start, end) range of an accepted rewrite.
    let mut covered: Vec<(usize, usize)> = Vec::new();

    for cand in &candidates {
        let is_nested = covered
            .iter()
            .any(|&(s, e)| cand.byte_start >= s && cand.byte_end <= e);
        if is_nested {
            overlapping.push(FieldAccessSite {
                line: cand.line,
                column: cand.column,
                kind: cand.kind.to_string(),
                context: cand.context.clone(),
            });
        } else {
            edits.push(TextEdit {
                byte_start: cand.byte_start,
                byte_end: cand.byte_end,
                replacement: cand.replacement.clone(),
            });
            covered.push((cand.byte_start, cand.byte_end));
            if cand.kind == "field_read" && cand.in_self_receiver {
                borrow_promotions.push(FieldAccessSite {
                    line: cand.line,
                    column: cand.column,
                    kind: "borrow_promotion".to_string(),
                    context: cand.context.clone(),
                });
            }
        }
    }

    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits).context("overlapping rewrites in update_rust_callers")?;

    // ── RX-W1b: warning marker emission for borrow_promotions ─────────────────
    let emit_applied_markers = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("emit_applied_markers"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let new_text_override: Option<String> = if emit_applied_markers
        && !borrow_promotions.is_empty()
        && !edits.is_empty()
    {
        let post_edit = apply_text_edits(&parsed.source, &edits)?;
        let site_lines: Vec<(usize, String)> = borrow_promotions
            .iter()
            .map(|site| {
                let field_name = site
                    .context
                    .strip_prefix("self.")
                    .and_then(|s| {
                        s.split(|c: char| !c.is_alphanumeric() && c != '_')
                            .next()
                    })
                    .unwrap_or("<unknown>");
                let marker = rust_warning_markers::emit_warning_marker(
                    "borrow promotion",
                    &format!(
                        "this delegate access now goes through &mut self.{delegate_field} \
                         even though the original read was through &self.{field_name}"
                    ),
                    "cross-check no concurrent borrow",
                );
                (site.line, marker)
            })
            .collect();
        Some(rust_warning_markers::apply_warning_markers_to_text(
            &post_edit,
            &site_lines,
        ))
    } else {
        None
    };

    let warning_count = if emit_applied_markers {
        borrow_promotions.len()
    } else {
        0
    };

    let fixme_count_override: Option<FixmeCount> = if warning_count > 0 {
        Some(FixmeCount {
            plan_only: 0,
            warning: warning_count,
        })
    } else {
        None
    };

    // ── assemble unrewriteable_accessors ──────────────────────────────────────
    let mut unrewriteable_accessors: Vec<RemainingFieldAccessor> = unrewriteable
        .into_iter()
        .map(|(field, accesses)| RemainingFieldAccessor { field, accesses })
        .collect();
    unrewriteable_accessors.sort_by(|a, b| a.field.cmp(&b.field));

    // ── assemble plan ─────────────────────────────────────────────────────────
    let sha = sha256_hex(parsed.source.as_bytes());
    let file_edits = if edits.is_empty() {
        Vec::new()
    } else {
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha,
            edits,
            new_text: new_text_override,
        }]
    };

    let validations = if file_edits.is_empty() {
        Vec::new()
    } else {
        vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }]
    };

    let plan = RefactorPlan {
        title: format!("rewrite callers through delegate `{delegate_field}`"),
        kind: "update_rust_callers".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
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
        fixme_count: fixme_count_override,
    };

    // validate_plan_shape rejects empty edits; skip it when nothing was rewritten.
    if !plan.edits.is_empty() {
        validate_plan_shape(&plan).context("validate update_rust_callers plan")?;
    }

    let response = PlanWithCallerExtra {
        plan,
        unrewriteable_accessors,
        borrow_promotions,
        overlapping_rewrite_sites: overlapping,
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

// ── field-type collection ─────────────────────────────────────────────────────

fn collect_struct_field_types_from_parsed(
    parsed: &ParsedSource,
    struct_name: &str,
    source_bytes: &[u8],
    out: &mut HashMap<String, String>,
) {
    let root = parsed.tree.root_node();
    let struct_item = rust_items(parsed)
        .into_iter()
        .find(|item| item.kind == "struct_item" && item.name.as_deref() == Some(struct_name));
    let struct_item = match struct_item {
        Some(s) => s,
        None => return,
    };
    let struct_node = match rust_node_by_range(
        root,
        "struct_item",
        struct_item.byte_start,
        struct_item.byte_end,
    ) {
        Some(n) => n,
        None => return,
    };
    collect_field_types_recursive(source_bytes, struct_node, out);
}

fn collect_field_types_recursive(
    source_bytes: &[u8],
    node: Node<'_>,
    out: &mut HashMap<String, String>,
) {
    if node.kind() == "field_declaration" {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("")
            .to_string();
        let ty = node
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            out.entry(name).or_insert(ty);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_field_types_recursive(source_bytes, child, out);
    }
}

// ── AST walk ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn walk_for_rewrites(
    source: &str,
    source_bytes: &[u8],
    node: Node<'_>,
    item_names: &HashSet<String>,
    field_types: &HashMap<String, String>,
    delegate_field: &str,
    candidates: &mut Vec<Candidate>,
    unrewriteable: &mut HashMap<String, Vec<FieldAccessSite>>,
) {
    match node.kind() {
        "call_expression" => {
            let fn_node = node.child_by_field_name("function");
            let is_moved_method = fn_node
                .filter(|f| f.kind() == "field_expression")
                .and_then(|f| {
                    let val = f.child_by_field_name("value")?;
                    let fld = f.child_by_field_name("field")?;
                    if val.utf8_text(source_bytes).ok()? != "self" {
                        return None;
                    }
                    let fname = fld.utf8_text(source_bytes).ok()?.to_string();
                    if item_names.contains(&fname) { Some(fname) } else { None }
                });

            if let Some(method_name) = is_moved_method {
                // The entire call_expression is the rewrite unit.
                let args_node = node.child_by_field_name("arguments");
                let args_text = args_node
                    .and_then(|n| n.utf8_text(source_bytes).ok())
                    .unwrap_or("()");
                let replacement =
                    format!("self.{delegate_field}.{method_name}{args_text}");
                let in_sr = is_in_self_receiver_fn(source_bytes, node);
                let (line, col) = line_col(source, node.start_byte());
                candidates.push(Candidate {
                    byte_start: node.start_byte(),
                    byte_end: node.end_byte(),
                    replacement,
                    name: method_name,
                    kind: "method_call",
                    line,
                    column: col,
                    context: line_text_at(source, node.start_byte()),
                    in_self_receiver: in_sr,
                });
                // Recurse into arguments: inner sites become overlapping in pass 2.
                if let Some(args) = args_node {
                    let mut cursor = args.walk();
                    for arg in args.named_children(&mut cursor) {
                        walk_for_rewrites(
                            source,
                            source_bytes,
                            arg,
                            item_names,
                            field_types,
                            delegate_field,
                            candidates,
                            unrewriteable,
                        );
                    }
                }
                return;
            }
            // Not a moved-method call — recurse normally.
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_for_rewrites(
                    source,
                    source_bytes,
                    child,
                    item_names,
                    field_types,
                    delegate_field,
                    candidates,
                    unrewriteable,
                );
            }
        }

        "field_expression" => {
            if let (Some(val), Some(fld)) = (
                node.child_by_field_name("value"),
                node.child_by_field_name("field"),
            ) {
                if val.utf8_text(source_bytes).ok() == Some("self") {
                    let fname = fld.utf8_text(source_bytes).unwrap_or("").to_string();
                    if item_names.contains(&fname) {
                        // Skip if this is the function part of a call_expression
                        // (already handled by the call_expression arm above).
                        if is_call_function(node) {
                            return;
                        }
                        let (line, col) = line_col(source, node.start_byte());
                        let ctx = line_text_at(source, node.start_byte());

                        // LHS of assignment / compound assignment → unrewriteable write.
                        if is_lhs_assignment(node) {
                            unrewriteable
                                .entry(fname)
                                .or_default()
                                .push(FieldAccessSite {
                                    line,
                                    column: col,
                                    kind: "write".to_string(),
                                    context: ctx,
                                });
                            return;
                        }

                        // RHS / rvalue: check Copy whitelist.
                        let field_ty = field_types.get(&fname).map(String::as_str).unwrap_or("");
                        if !field_ty.is_empty() && rust_deep::is_copy_whitelist(field_ty) {
                            let replacement =
                                format!("self.{delegate_field}.{fname}()");
                            let in_sr = is_in_self_receiver_fn(source_bytes, node);
                            candidates.push(Candidate {
                                byte_start: node.start_byte(),
                                byte_end: node.end_byte(),
                                replacement,
                                name: fname,
                                kind: "field_read",
                                line,
                                column: col,
                                context: ctx,
                                in_self_receiver: in_sr,
                            });
                        } else {
                            // Not Copy or type unknown.
                            unrewriteable
                                .entry(fname)
                                .or_default()
                                .push(FieldAccessSite {
                                    line,
                                    column: col,
                                    kind: "read".to_string(),
                                    context: ctx,
                                });
                        }
                        return;
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_for_rewrites(
                    source,
                    source_bytes,
                    child,
                    item_names,
                    field_types,
                    delegate_field,
                    candidates,
                    unrewriteable,
                );
            }
        }

        "struct_pattern" | "struct_expression" => {
            // Pattern / struct-literal destructuring — report spread and field patterns.
            report_struct_pattern_sites(
                source,
                source_bytes,
                node,
                item_names,
                unrewriteable,
            );
            // Don't recurse further; the pattern sites are fully handled.
        }

        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_for_rewrites(
                    source,
                    source_bytes,
                    child,
                    item_names,
                    field_types,
                    delegate_field,
                    candidates,
                    unrewriteable,
                );
            }
        }
    }
}

// ── structural pattern reporting ──────────────────────────────────────────────

fn report_struct_pattern_sites(
    source: &str,
    source_bytes: &[u8],
    node: Node<'_>,
    item_names: &HashSet<String>,
    unrewriteable: &mut HashMap<String, Vec<FieldAccessSite>>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "field_pattern" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let fname = name_node.utf8_text(source_bytes).unwrap_or("").to_string();
                    if item_names.contains(&fname) {
                        let (line, col) = line_col(source, child.start_byte());
                        let ctx = line_text_at(source, child.start_byte());
                        unrewriteable
                            .entry(fname)
                            .or_default()
                            .push(FieldAccessSite {
                                line,
                                column: col,
                                kind: "pattern_destructure".to_string(),
                                context: ctx,
                            });
                    }
                }
            }
            "remaining_fields" => {
                let (line, col) = line_col(source, child.start_byte());
                let ctx = line_text_at(source, child.start_byte());
                for fname in item_names {
                    unrewriteable
                        .entry(fname.clone())
                        .or_default()
                        .push(FieldAccessSite {
                            line,
                            column: col,
                            kind: "spread".to_string(),
                            context: ctx.clone(),
                        });
                }
            }
            _ => {}
        }
    }
}

// ── classification helpers ────────────────────────────────────────────────────

/// Returns true when the given `field_expression` is the function of a `call_expression`.
fn is_call_function(node: Node<'_>) -> bool {
    if let Some(parent) = node.parent() {
        if parent.kind() == "call_expression" {
            if let Some(fn_node) = parent.child_by_field_name("function") {
                return fn_node.start_byte() == node.start_byte()
                    && fn_node.end_byte() == node.end_byte();
            }
        }
    }
    false
}

/// Returns true when the given node is the left-hand side of an assignment
/// or compound-assignment expression.
fn is_lhs_assignment(node: Node<'_>) -> bool {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            "assignment_expression" | "compound_assignment_expr" => {
                if let Some(left) = parent.child_by_field_name("left") {
                    return left.start_byte() == node.start_byte()
                        && left.end_byte() == node.end_byte();
                }
            }
            _ => {}
        }
    }
    false
}

/// Returns true when the node is inside a function whose first parameter is
/// `&self` (shared reference receiver, not `&mut self`).
fn is_in_self_receiver_fn(source_bytes: &[u8], node: Node<'_>) -> bool {
    let mut p = node.parent();
    while let Some(parent) = p {
        if parent.kind() == "function_item" {
            if let Some(params) = parent.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for param in params.named_children(&mut cursor) {
                    if param.kind() == "self_parameter" {
                        let text = param.utf8_text(source_bytes).unwrap_or("");
                        return text.starts_with('&') && !text.contains("mut");
                    }
                    break;
                }
            }
            return false;
        }
        p = parent.parent();
    }
    false
}

/// Extract the trimmed text of the line containing `byte`.
fn line_text_at(source: &str, byte: usize) -> String {
    let start = {
        let limited = byte.min(source.len());
        source[..limited].rfind('\n').map(|i| i + 1).unwrap_or(0)
    };
    let end = source[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(source.len());
    source.get(start..end).unwrap_or("").trim().to_string()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(
        source: &std::path::Path,
        struct_name: &str,
        delegate_field: &str,
        item_names: &[&str],
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "update_rust_callers".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: Some(item_names.iter().map(|&s| s.to_string()).collect()),
            item_kinds: None,
            impl_name: Some(struct_name.to_string()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: Some(delegate_field.to_string()),
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            rewrite_remaining_accessors: None,
            boolean_getter_strategy: None,
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            wiring_mode: None,
            callback_externals: None,
            output_path: None,
        }
    }

    fn apply_plan(plan_json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        let edits_arr = v["edits"].as_array().unwrap();
        if edits_arr.is_empty() {
            // No edits — return original source text.
            let src = v["edits"].as_array();
            let _ = src;
            return String::new();
        }
        let first_file = &edits_arr[0];
        let path = first_file["path"].as_str().unwrap();
        let source = std::fs::read_to_string(path).unwrap();
        let edits: Vec<TextEdit> = serde_json::from_value(first_file["edits"].clone()).unwrap();
        apply_text_edits(&source, &edits).unwrap()
    }

    // Gate: Copy-whitelisted rvalue field read rewrites cleanly.
    #[test]
    fn update_callers_copy_rvalue_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn get_count(&self) -> u32 {
        self.count
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state.count()"),
            "expected rewritten accessor:\n{result}"
        );
        assert!(
            !result.contains("self.count\n") && !result.contains("self.count;"),
            "expected old access removed:\n{result}"
        );
    }

    // Gate: field write goes to unrewriteable_accessors, NOT in FileEdits.
    #[test]
    fn update_callers_write_unrewriteable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn set_count(&mut self) {
        self.count = 5;
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        // No FileEdits (nothing safely rewriteable).
        assert!(
            v["edits"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "expected no file edits for write site"
        );
        let unrewriteable = v["unrewriteable_accessors"].as_array().unwrap();
        assert!(
            !unrewriteable.is_empty(),
            "expected write site in unrewriteable_accessors"
        );
        let count_entry = unrewriteable
            .iter()
            .find(|e| e["field"] == "count")
            .expect("expected count in unrewriteable_accessors");
        let kinds: Vec<&str> = count_entry["accesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["kind"].as_str().unwrap())
            .collect();
        assert!(
            kinds.contains(&"write"),
            "expected write kind: {kinds:?}"
        );
    }

    // Gate: method call on moved method rewrites cleanly.
    #[test]
    fn update_callers_method_call_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    state: ServerState,
}

impl BigServer {
    fn run(&self) {
        self.helper();
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["helper"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state.helper()"),
            "expected rewritten method call:\n{result}"
        );
    }

    // Gate: compound assignment goes to unrewriteable_accessors.
    #[test]
    fn update_callers_compound_unrewriteable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn inc(&mut self) {
        self.count += 1;
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        assert!(
            v["edits"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "expected no file edits for compound assignment"
        );
        let unrewriteable = v["unrewriteable_accessors"].as_array().unwrap();
        assert!(!unrewriteable.is_empty());
    }

    // Gate: non-Copy field read goes to unrewriteable_accessors.
    #[test]
    fn update_callers_non_copy_unrewriteable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    name: String,
    state: ServerState,
}

impl BigServer {
    fn get_name(&self) -> &str {
        &self.name
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["name"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        assert!(
            v["edits"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "expected no file edits for non-Copy field"
        );
        let unrewriteable = v["unrewriteable_accessors"].as_array().unwrap();
        assert!(
            !unrewriteable.is_empty(),
            "expected String field in unrewriteable_accessors"
        );
    }

    // Gate: Copy-whitelisted read inside &self fn appears in borrow_promotions.
    #[test]
    fn update_callers_borrow_promotion_detected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn peek(&self) -> u32 {
        self.count
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        // Should have a rewrite (count is Copy).
        assert!(
            !v["edits"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "expected a rewrite edit for u32 count"
        );
        // And the rewrite site should appear in borrow_promotions.
        let promotions = v["borrow_promotions"].as_array().unwrap();
        assert!(
            !promotions.is_empty(),
            "expected borrow_promotion for &self read"
        );
    }

    // Gate: nested self call — outer rewritten, inner in overlapping_rewrite_sites.
    #[test]
    fn update_callers_nested_overlap_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn forward(&self) {
        self.helper(self.count);
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", &["helper", "count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        // The outer call self.helper(...) should be rewritten.
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state.helper("),
            "expected outer call rewritten:\n{result}"
        );
        // The inner self.count inside the args should be in overlapping_rewrite_sites.
        let overlapping = v["overlapping_rewrite_sites"].as_array().unwrap();
        assert!(
            !overlapping.is_empty(),
            "expected overlapping_rewrite_sites for inner self.count"
        );
    }

    // Gate: sibling args both rewritten independently.
    #[test]
    fn update_callers_sibling_args_both_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    state: ServerState,
}

impl BigServer {
    fn go(&self) {
        external(self.helper(), self.count);
    }
}
"#,
        )
        .unwrap();
        // helper is in moved set (method); count is Copy field.
        let p = make_params(&src, "BigServer", "state", &["helper", "count"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state.helper()"),
            "expected helper rewritten:\n{result}"
        );
        assert!(
            result.contains("self.state.count()"),
            "expected count rewritten:\n{result}"
        );
    }

    // Gate: no matching items → empty edits, valid response.
    #[test]
    fn update_callers_no_matches_empty_plan() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn get(&self) -> u32 {
        self.count
    }
}
"#,
        )
        .unwrap();
        // item_names doesn't include anything in the source.
        let p = make_params(&src, "BigServer", "state", &["nonexistent"]);
        let plan_json = plan_update_callers(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(
            v["kind"].as_str().unwrap(),
            "update_rust_callers",
            "expected correct kind in response"
        );
        assert!(
            v["edits"].as_array().map(|a| a.is_empty()).unwrap_or(true),
            "expected empty edits"
        );
    }
}

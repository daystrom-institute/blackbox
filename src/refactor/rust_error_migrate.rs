use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use tree_sitter::Node;

use super::*;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QuestionMarkSite {
    line: usize,
    column: usize,
    classification: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct PlanWithQuestionMarkSites {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    question_mark_sites: Vec<QuestionMarkSite>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    operator_opt_outs_used: Vec<String>,
}

pub fn plan_rewrite_error_type(p: &crate::refactor::RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_rust_file(&source_path)?;

    let old_text = p
        .old_text
        .as_deref()
        .ok_or_else(|| anyhow!("old_text is required for rewrite_error_type"))?;
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for rewrite_error_type"))?;
    let item_names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for rewrite_error_type"))?;

    let error_mapping: HashMap<String, String> = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("error_mapping"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let acknowledge_public_api_change = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("acknowledge_public_api_change"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let source = &parsed.source;
    let root = parsed.tree.root_node();

    let mut edits = Vec::new();
    let mut question_mark_sites = Vec::new();
    let mut operator_opt_outs = Vec::new();

    if !acknowledge_public_api_change && is_error_type_pub(root, source, old_text) {
        bail!(
            "error.bad_input(code=public_api_change_unacknowledged): \
             error type `{old_text}` is pub in {}; \
             set acknowledge_public_api_change=true to proceed",
            source_path.display()
        );
    }
    if acknowledge_public_api_change {
        operator_opt_outs.push("acknowledge_public_api_change".to_string());
    }

    let has_from = has_from_impl_for(source, new_text);

    // Phase 1: process named functions — signature rewrite + downcast check
    for func_name in item_names {
        let func_node = find_function_by_name(root, source, func_name).ok_or_else(|| {
            anyhow!(
                "function `{func_name}` not found in {}",
                source_path.display()
            )
        })?;

        if has_downcast_in_body(func_node, source, old_text) {
            bail!(
                "error.bad_input(code=error_downcast_unsupported): \
                 downcast/downcast_ref on error type in `{func_name}`"
            );
        }

        if let Some(ret_type) = func_node.child_by_field_name("return_type") {
            if let Some(old_err_node) = find_old_err_in_type(ret_type, source, old_text) {
                edits.push(TextEdit {
                    byte_start: old_err_node.start_byte(),
                    byte_end: old_err_node.end_byte(),
                    replacement: new_text.to_string(),
                });
            }
        }
    }

    // Phase 2: walk ALL function bodies for construction sites and ? sites
    // Use text-based scanning for construction sites to handle macro content.
    // Use text-based scanning for ? to cover functions not in item_names.
    let all_funcs = collect_all_functions(root);
    for func_node in &all_funcs {
        let Some(body) = func_node.child_by_field_name("body") else {
            continue;
        };
        let body_start = body.start_byte();
        let body_end = body.end_byte();
        let body_text = &source[body_start..body_end];

        // Text-based construction mapping: find OldErr::VariantName patterns
        // and replace entire scoped-id with NewErr::MappedName
        let prefix = format!("{}::", old_text);
        let mut search_pos = 0usize;
        while let Some(rel) = body_text[search_pos..].find(&prefix) {
            let abs = body_start + search_pos + rel;
            let after = abs + old_text.len() + 2;
            if after >= source.len() {
                break;
            }
            let remaining = &source[after..];
            let variant_end = remaining
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(remaining.len());
            let variant_name = &remaining[..variant_end];
            if let Some(mapped_name) = error_mapping.get(variant_name) {
                edits.push(TextEdit {
                    byte_start: abs,
                    byte_end: abs + old_text.len() + 2 + variant_end,
                    replacement: format!("{}::{}", new_text, mapped_name),
                });
            }
            search_pos += rel + prefix.len();
        }

        // Text-based ? detection
        for (rel_offset, ch) in body_text.char_indices() {
            if ch == '?' {
                let abs = body_start + rel_offset;
                let (line, col) = line_col(source, abs);
                question_mark_sites.push(QuestionMarkSite {
                    line,
                    column: col,
                    classification: if has_from {
                        "text_compatible".to_string()
                    } else {
                        "unknown".to_string()
                    },
                });
            }
        }
    }

    if edits.is_empty() {
        bail!(
            "no rewrite_error_type edits found in {}",
            source_path.display()
        );
    }

    ensure_non_overlapping(&edits).context("overlapping edits in rewrite_error_type")?;

    let plan = RefactorPlan {
        title: format!(
            "rewrite error type `{old_text}` to `{new_text}` in {}",
            path_string(&source_path)
        ),
        kind: "rewrite_rust_error_type".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
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

    validate_plan_shape(&plan).context("failed to validate rewrite_error_type plan")?;
    let response = PlanWithQuestionMarkSites {
        plan,
        question_mark_sites,
        operator_opt_outs_used: operator_opt_outs,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

fn collect_all_functions(root: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_item" {
            out.push(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

fn find_function_by_name<'a>(root: Node<'a>, source: &'a str, name: &str) -> Option<Node<'a>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_item" {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(n) = name_node.utf8_text(source.as_bytes()) {
                    if n == name {
                        return Some(node);
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

/// Detect .downcast() or .downcast_ref() calls referencing old_text.
/// Handles both plain field_expression (.downcast()) and
/// generic_function (.downcast::<OldErr>()) forms.
fn has_downcast_in_body(func_node: Node<'_>, source: &str, old_text: &str) -> bool {
    let Some(body) = func_node.child_by_field_name("body") else {
        return false;
    };
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let (field_name, type_args_node) = match func.kind() {
                    "field_expression" => {
                        let field_name = func
                            .child_by_field_name("field")
                            .and_then(|f| f.utf8_text(source.as_bytes()).ok());
                        let type_args = node.child_by_field_name("type_arguments");
                        (field_name, type_args)
                    }
                    "generic_function" => {
                        let inner = func.child_by_field_name("function");
                        let field_name = inner.and_then(|inner| {
                            if inner.kind() == "field_expression" {
                                inner
                                    .child_by_field_name("field")
                                    .and_then(|f| f.utf8_text(source.as_bytes()).ok())
                            } else {
                                None
                            }
                        });
                        let type_args = func.child_by_field_name("type_arguments");
                        (field_name, type_args)
                    }
                    _ => (None, None),
                };
                if let Some(name) = field_name {
                    if name == "downcast" || name == "downcast_ref" {
                        if let Some(type_args) = type_args_node {
                            if let Ok(text) = type_args.utf8_text(source.as_bytes()) {
                                if text.contains(old_text) {
                                    return true;
                                }
                            }
                        } else {
                            return true;
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn find_old_err_in_type<'a>(
    container: Node<'a>,
    source: &'a str,
    old_text: &str,
) -> Option<Node<'a>> {
    let mut stack = vec![container];
    while let Some(node) = stack.pop() {
        if node.kind() == "type_identifier" {
            if let Ok(text) = node.utf8_text(source.as_bytes()) {
                if text == old_text {
                    return Some(node);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn is_error_type_pub(root: Node<'_>, source: &str, old_text: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "type_item" | "enum_item" | "struct_item") {
            let has_pub = {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .any(|c| c.kind() == "visibility_modifier")
            };
            if has_pub {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Ok(name) = name_node.utf8_text(source.as_bytes()) {
                        if name == old_text {
                            return true;
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn has_from_impl_for(source: &str, new_text: &str) -> bool {
    let needle = format!("for {}", new_text);
    source.contains("From<") && source.contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_response(plan_json: &str) -> (Vec<TextEdit>, Vec<QuestionMarkSite>, Vec<String>) {
        let value: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        let edits: Vec<TextEdit> = value
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|files| {
                files
                    .iter()
                    .flat_map(|file| {
                        file.get("edits")
                            .and_then(|e| e.as_array())
                            .cloned()
                            .unwrap_or_default()
                    })
                    .map(|edit_val| serde_json::from_value::<TextEdit>(edit_val).unwrap())
                    .collect()
            })
            .unwrap_or_default();
        let qm_sites: Vec<QuestionMarkSite> = value
            .get("question_mark_sites")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|v| QuestionMarkSite {
                        line: v["line"].as_u64().unwrap_or(0) as usize,
                        column: v["column"].as_u64().unwrap_or(0) as usize,
                        classification: v["classification"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let opt_outs: Vec<String> = value
            .get("operator_opt_outs_used")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        (edits, qm_sites, opt_outs)
    }

    fn plan_for(
        source: &std::path::Path,
        old_text: &str,
        new_text: &str,
        item_names: &[&str],
        error_mapping: &[(&str, &str)],
        acknowledge_public_api_change: bool,
    ) -> Result<String> {
        let mapping: HashMap<String, String> = error_mapping
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let toml_entries = if mapping.is_empty() && !acknowledge_public_api_change {
            None
        } else {
            let mut map = serde_json::Map::new();
            if !mapping.is_empty() {
                let mapping_json = serde_json::to_value(&mapping).expect("mapping serialization");
                map.insert("error_mapping".to_string(), mapping_json);
            }
            if acknowledge_public_api_change {
                map.insert(
                    "acknowledge_public_api_change".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            Some(
                map.into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
        };
        plan_rewrite_error_type(&crate::refactor::RefactorPlanParams {
            kind: "rewrite_rust_error_type".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: Some(item_names.iter().map(|s| s.to_string()).collect()),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some(old_text.to_string()),
            new_text: Some(new_text.to_string()),
            replace_all: None,
            toml_table: None,
            toml_entries,
            project_dir: None,
            fields: None,
            parameters: None,
            assign_to_fields: None,
            move_fields: None,
            delegate_field: None,
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
            ..Default::default()
        })
    }

    #[test]
    fn rewrite_error_type_function_signature() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn do_something() -> Result<(), OldErr> {
    Ok(())
}

fn other_fn() -> Result<i32, OldErr> {
    Ok(42)
}
"#,
        )
        .unwrap();

        let result = plan_for(
            &source,
            "OldErr",
            "NewErr",
            &["do_something", "other_fn"],
            &[],
            false,
        );
        let plan_json = result.unwrap();
        let (edits, qm_sites, opt_outs) = parse_response(&plan_json);
        assert!(qm_sites.is_empty());
        assert!(opt_outs.is_empty());
        assert_eq!(edits.len(), 2);
        let source_text = std::fs::read_to_string(&source).unwrap();
        let rewritten = super::apply_text_edits(&source_text, &edits).unwrap();
        assert!(rewritten.contains("fn do_something() -> Result<(), NewErr>"));
        assert!(rewritten.contains("fn other_fn() -> Result<i32, NewErr>"));
    }

    #[test]
    fn rewrite_error_type_construction_via_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
use anyhow::bail;

fn do_something() -> Result<(), OldErr> {
    bail!(OldErr::IoFail);
    bail!(OldErr::NotFound);
}

fn unmapped() -> Result<(), OldErr> {
    bail!(OldErr::UnmappedVariant);
}
"#,
        )
        .unwrap();

        let result = plan_for(
            &source,
            "OldErr",
            "NewErr",
            &["do_something", "unmapped"],
            &[("IoFail", "Io"), ("NotFound", "Missing")],
            false,
        );
        let plan_json = result.unwrap();
        let (edits, _, _) = parse_response(&plan_json);
        let source_text = std::fs::read_to_string(&source).unwrap();
        let rewritten = super::apply_text_edits(&source_text, &edits).unwrap();
        assert!(rewritten.contains("bail!(NewErr::Io)"));
        assert!(rewritten.contains("bail!(NewErr::Missing)"));
        assert!(rewritten.contains("bail!(OldErr::UnmappedVariant)"));
    }

    #[test]
    fn rewrite_error_type_question_mark_sites() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn inner() -> Result<i32, OldErr> { Ok(42) }

impl From<OtherErr> for NewErr {
    fn from(e: OtherErr) -> Self { NewErr::Other }
}

fn with_question() -> Result<(), NewErr> {
    let x = inner()?;
    println!("{x}");
    Ok(())
}
"#,
        )
        .unwrap();

        let result = plan_for(&source, "OldErr", "NewErr", &["inner"], &[], false);
        let plan_json = result.unwrap();
        let (edits, qm_sites, _) = parse_response(&plan_json);
        assert_eq!(edits.len(), 1);
        assert!(!qm_sites.is_empty());
        for site in &qm_sites {
            assert_eq!(site.classification, "text_compatible");
        }
    }

    #[test]
    fn rewrite_error_type_question_mark_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn inner() -> Result<i32, OldErr> { Ok(42) }

fn with_question() -> Result<(), NewErr> {
    let x = inner()?;
    println!("{x}");
    Ok(())
}
"#,
        )
        .unwrap();

        let result = plan_for(&source, "OldErr", "NewErr", &["inner"], &[], false);
        let plan_json = result.unwrap();
        let (edits, qm_sites, _) = parse_response(&plan_json);
        assert_eq!(edits.len(), 1);
        if !qm_sites.is_empty() {
            for site in &qm_sites {
                assert_eq!(site.classification, "unknown");
            }
        }
    }

    #[test]
    fn rewrite_error_type_downcast_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn with_downcast() -> Result<(), OldErr> {
    let err: Box<dyn std::error::Error> = Box::new(OldErr::IoFail);
    let specific = err.downcast::<OldErr>().unwrap();
    Ok(specific)
}
"#,
        )
        .unwrap();

        let err = plan_for(
            &source,
            "OldErr",
            "NewErr",
            &["with_downcast"],
            &[("IoFail", "Io")],
            false,
        );
        let msg = err.expect_err("downcast should refuse").to_string();
        assert!(
            msg.contains("error.bad_input(code=error_downcast_unsupported)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn rewrite_error_type_pub_type_no_ack_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
pub enum OldErr {
    IoFail,
}

fn do_something() -> Result<(), OldErr> {
    Ok(())
}
"#,
        )
        .unwrap();

        let err = plan_for(&source, "OldErr", "NewErr", &["do_something"], &[], false);
        let msg = err
            .expect_err("pub type without ack should refuse")
            .to_string();
        assert!(
            msg.contains("error.bad_input(code=public_api_change_unacknowledged)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn rewrite_error_type_pub_type_with_ack_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
pub enum OldErr {
    IoFail,
}

fn do_something() -> Result<(), OldErr> {
    Ok(())
}
"#,
        )
        .unwrap();

        let result = plan_for(&source, "OldErr", "NewErr", &["do_something"], &[], true);
        let plan_json = result.unwrap();
        let (edits, _, opt_outs) = parse_response(&plan_json);
        assert_eq!(edits.len(), 1);
        assert!(opt_outs.contains(&"acknowledge_public_api_change".to_string()));
        let source_text = std::fs::read_to_string(&source).unwrap();
        let rewritten = super::apply_text_edits(&source_text, &edits).unwrap();
        assert!(rewritten.contains("fn do_something() -> Result<(), NewErr>"));
    }

    #[test]
    fn rewrite_error_type_non_pub_type_no_ack_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
enum OldErr {
    IoFail,
}

fn do_something() -> Result<(), OldErr> {
    Ok(())
}
"#,
        )
        .unwrap();

        let result = plan_for(&source, "OldErr", "NewErr", &["do_something"], &[], false);
        let plan_json = result.unwrap();
        let (edits, _, opt_outs) = parse_response(&plan_json);
        assert_eq!(edits.len(), 1);
        assert!(opt_outs.is_empty());
    }
}

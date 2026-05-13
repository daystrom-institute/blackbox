//! `extract_rust_function_region` plan kind.
//!
//! Conservative intra-function extraction. The operator provides an exact
//! selected region plus the helper signature surface; the planner finds the
//! enclosing function, rejects control-flow-heavy regions, inserts a helper
//! beside the enclosing function, and replaces the region with a call.

use anyhow::{Result, anyhow, bail};
use tree_sitter::Node;

use super::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, parse_rust_file, path_string, resolve_path, rust_decl_visibility_prefix,
    sha256_hex, validate_plan_shape, validate_rust_identifier,
};

pub(crate) fn plan_extract_function_region(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_rust_file(&source_path)?;
    let selected = p
        .old_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("old_text is required for extract_rust_function_region"))?;
    reject_complex_region(selected)?;
    let helper_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .or(p.module_name.as_ref())
        .ok_or_else(|| {
            anyhow!("item_names[0] or module_name is required for extract_rust_function_region")
        })?;
    validate_rust_identifier(helper_name, "helper_name")?;

    let matches = selected_matches(&parsed.source, selected);
    if matches.len() != 1 {
        bail!(
            "old_text must match exactly once in {}; found {} matches",
            source_path.display(),
            matches.len()
        );
    }
    let (region_start, region_end) = matches[0];
    let enclosing_fn =
        find_enclosing_function(parsed.tree.root_node(), region_start, region_end)
            .ok_or_else(|| anyhow!("old_text is not fully enclosed by a Rust function_item"))?;
    let helper_is_associated = function_inside_impl(enclosing_fn);
    let insert_at = helper_insert_byte(enclosing_fn)?;

    let params = toml_str_array(&p.toml_entries, "parameters");
    let args = toml_str_array(&p.toml_entries, "arguments");
    let return_type = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("return_type"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "()");
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let replacement = p.new_text.clone().unwrap_or_else(|| {
        let callee = if helper_is_associated {
            format!("Self::{helper_name}")
        } else {
            helper_name.to_string()
        };
        let call = format!("{callee}({})", args.join(", "));
        if return_type.is_some() {
            call
        } else {
            format!("{call};")
        }
    });
    let helper = render_helper(visibility, helper_name, &params, return_type, selected);
    let insert_text = if parsed.source[..insert_at].ends_with("\n\n") {
        format!("{helper}\n")
    } else {
        format!("\n\n{helper}\n")
    };

    let plan = RefactorPlan {
        title: format!(
            "extract Rust function region from {} into {helper_name}",
            path_string(&source_path)
        ),
        kind: "extract_rust_function_region".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![
                TextEdit {
                    byte_start: region_start,
                    byte_end: region_end,
                    replacement,
                },
                TextEdit {
                    byte_start: insert_at,
                    byte_end: insert_at,
                    replacement: insert_text,
                },
            ],
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
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
    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn selected_matches(source: &str, selected: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(relative) = source[start..].find(selected) {
        let byte_start = start + relative;
        out.push((byte_start, byte_start + selected.len()));
        start = byte_start + selected.len();
    }
    out
}

fn find_enclosing_function<'a>(
    node: Node<'a>,
    region_start: usize,
    region_end: usize,
) -> Option<Node<'a>> {
    if node.start_byte() <= region_start
        && node.end_byte() >= region_end
        && node.kind() == "function_item"
    {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(found) = find_enclosing_function(child, region_start, region_end) {
                return Some(found);
            }
        }
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() <= region_start
            && child.end_byte() >= region_end
            && let Some(found) = find_enclosing_function(child, region_start, region_end)
        {
            return Some(found);
        }
    }
    None
}

fn helper_insert_byte(function_node: Node<'_>) -> Result<usize> {
    let mut node = function_node;
    while let Some(parent) = node.parent() {
        if parent.kind() == "declaration_list" || parent.kind() == "source_file" {
            return Ok(function_node.end_byte());
        }
        node = parent;
    }
    Ok(function_node.end_byte())
}

fn function_inside_impl(function_node: Node<'_>) -> bool {
    let mut node = function_node;
    while let Some(parent) = node.parent() {
        if parent.kind() == "impl_item" {
            return true;
        }
        node = parent;
    }
    false
}

fn render_helper(
    visibility: &str,
    helper_name: &str,
    params: &[String],
    return_type: Option<&str>,
    selected: &str,
) -> String {
    let return_suffix = return_type
        .map(|ty| format!(" -> {ty}"))
        .unwrap_or_default();
    let body = indent_region(selected.trim_matches('\n'));
    format!(
        "{visibility}fn {helper_name}({}){return_suffix} {{\n{body}\n}}",
        params.join(", ")
    )
}

fn indent_region(region: &str) -> String {
    region
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("    {}", line.trim_start())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reject_complex_region(region: &str) -> Result<()> {
    for needle in ["return", "break", "continue"] {
        if contains_word(region, needle) {
            bail!(
                "extract_rust_function_region rejects regions containing `{needle}`; extract a simpler expression/statement block"
            );
        }
    }
    if region.contains('?') {
        bail!("extract_rust_function_region rejects `?`; provide an explicit helper manually");
    }
    Ok(())
}

fn contains_word(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(idx, _)| {
        let before = text.as_bytes().get(idx.wrapping_sub(1)).copied();
        let after = text.as_bytes().get(idx + needle.len()).copied();
        rust_word_boundary(before) && rust_word_boundary(after)
    })
}

fn rust_word_boundary(ch: Option<u8>) -> bool {
    !matches!(ch, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn toml_str_array(
    entries: &Option<std::collections::BTreeMap<String, serde_json::Value>>,
    key: &str,
) -> Vec<String> {
    entries
        .as_ref()
        .and_then(|entries| entries.get(key))
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use tree_sitter::Node;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrateReplacementKind {
    BareConcrete,
    BoxDyn,
    ArcDyn,
    RcDyn,
    ImplTrait,
    GenericParamTBoundedTrait,
}

impl MigrateReplacementKind {
    fn parse(raw: &str) -> Result<Self> {
        let normalized = raw
            .trim()
            .replace('-', "")
            .replace('_', "")
            .replace(' ', "")
            .to_ascii_lowercase();

        match normalized.as_str() {
            "bareconcrete" => Ok(Self::BareConcrete),
            "boxdyn" => Ok(Self::BoxDyn),
            "arcdyn" => Ok(Self::ArcDyn),
            "rcdyn" => Ok(Self::RcDyn),
            "impltrait" => Ok(Self::ImplTrait),
            "genericparamtboundedtrait" => Ok(Self::GenericParamTBoundedTrait),
            _ => bail!("unsupported replacement_kind `{raw}`"),
        }
    }

    fn replacement_text(self, new_text: &str) -> &str {
        match self {
            Self::GenericParamTBoundedTrait => "T",
            _ => new_text,
        }
    }

    fn is_position_legal(self, position: TypeUsagePosition) -> bool {
        match self {
            Self::BareConcrete => true,
            Self::BoxDyn | Self::ArcDyn | Self::RcDyn => {
                matches!(
                    position,
                    TypeUsagePosition::Field
                        | TypeUsagePosition::FunctionParam
                        | TypeUsagePosition::FunctionReturn
                        | TypeUsagePosition::TypeAlias
                        | TypeUsagePosition::GenericArg
                )
            }
            Self::ImplTrait => {
                matches!(
                    position,
                    TypeUsagePosition::FunctionParam | TypeUsagePosition::FunctionReturn
                )
            }
            Self::GenericParamTBoundedTrait => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeUsagePosition {
    Field,
    FunctionParam,
    FunctionReturn,
    TypeAlias,
    GenericArg,
    LocalBinding,
    Unsupported,
}

#[derive(Debug, Clone)]
struct GenericAnchor {
    start_byte: usize,
    end_byte: usize,
}

#[derive(Debug, Clone)]
struct MigrationCandidate {
    node_start: usize,
    node_end: usize,
    line: usize,
    column: usize,
    position: TypeUsagePosition,
    anchor: Option<GenericAnchor>,
}

#[derive(Debug, Clone, Serialize)]
struct MigrationSkippedSite {
    line: usize,
    column: usize,
    reason: String,
}

#[derive(Debug, Serialize)]
struct PlanWithMigrationSkipped {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    migration_skipped: Vec<MigrationSkippedSite>,
}

pub fn plan_migrate_type_usages(
    p: &crate::refactor::RefactorPlanParams,
) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_rust_file(&source_path)?;

    let module_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name is required for migrate_type_usages"))?;
    if module_name.is_empty() {
        bail!("module_name must not be empty");
    }
    validate_rust_identifier(module_name, "module_name")?;

    let replacement_kind = MigrateReplacementKind::parse(
        p.old_text
            .as_deref()
            .ok_or_else(|| anyhow!("old_text is required for migrate_type_usages"))?,
    )?;
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for migrate_type_usages"))?;
    if new_text.trim().is_empty() {
        bail!("new_text must not be empty");
    }

    let root = parsed.tree.root_node();
    let (accepted, mut migration_skipped) = collect_type_sites(root, &parsed.source, module_name);

    if accepted.is_empty() && migration_skipped.is_empty() {
        bail!(
            "no migrate_type_usages candidates found for `{}` in {}",
            module_name,
            source_path.display()
        );
    }

    let mut legal_sites = Vec::new();
    let mut illegal_sites = Vec::new();
    for candidate in accepted {
        if replacement_kind.is_position_legal(candidate.position) {
            legal_sites.push(candidate);
        } else {
            illegal_sites.push(candidate);
        }
    }

    if replacement_kind == MigrateReplacementKind::ImplTrait && !illegal_sites.is_empty() {
        bail!(
            "error.bad_input(code=impl_trait_illegal_position): {}",
            summarize_sites(&illegal_sites)
        );
    }

    let mut edits = legal_sites
        .iter()
        .map(|candidate| TextEdit {
            byte_start: candidate.node_start,
            byte_end: candidate.node_end,
            replacement: replacement_kind.replacement_text(new_text).to_string(),
        })
        .collect::<Vec<_>>();

    if replacement_kind == MigrateReplacementKind::GenericParamTBoundedTrait {
        let mut anchors: HashMap<(usize, usize), Vec<&MigrationCandidate>> = HashMap::new();
        for candidate in &legal_sites {
            if let Some(anchor) = candidate.anchor.as_ref() {
                anchors
                    .entry((anchor.start_byte, anchor.end_byte))
                    .or_default()
                    .push(candidate);
            } else {
                illegal_sites.push(MigrationCandidate {
                    node_start: candidate.node_start,
                    node_end: candidate.node_end,
                    line: candidate.line,
                    column: candidate.column,
                    position: candidate.position,
                    anchor: None,
                });
            }
        }

        let mut conflict_sites = Vec::new();
        for ((start, end), _sites) in anchors {
            let anchor_node = find_node_by_range(root, start, end)
                .with_context(|| format!("failed to resolve generic anchor {}..{}", start, end))?;

            if has_generic_type_param_t(&anchor_node, &parsed.source) {
                let reasons = collect_line_cols_for_anchor_range(&legal_sites, start, end)
                    .into_iter()
                    .map(|(line, column)| MigrationSkippedSite {
                        line,
                        column,
                        reason: "generic_param_conflict".to_string(),
                    })
                    .collect::<Vec<_>>();
                conflict_sites.extend(reasons);
                continue;
            }

            edits.push(build_add_generic_param_edit(&anchor_node, &parsed.source)?);
            if let Some(where_clause_edit) =
                build_where_clause_edit(&anchor_node, &parsed.source, new_text)
            {
                edits.push(where_clause_edit);
            }
        }

        if !conflict_sites.is_empty() {
            migration_skipped.extend(conflict_sites);
            bail!(
                "error.bad_input(code=generic_param_conflict): {}",
                join_locations_struct(&migration_skipped)
            );
        }
    }

    if !matches!(replacement_kind, MigrateReplacementKind::GenericParamTBoundedTrait)
        && !illegal_sites.is_empty()
    {
        migration_skipped.extend(
            illegal_sites
                .iter()
                .map(|candidate| MigrationSkippedSite {
                    line: candidate.line,
                    column: candidate.column,
                    reason: "unsupported_type_position".to_string(),
                })
                .collect::<Vec<_>>(),
        );
    }

    let mut canonical_edits = canonicalize_and_merge(edits)?;
    ensure_non_overlapping(&canonical_edits)
        .context("overlapping edits in migrate_rust_type_usages")?;

    if canonical_edits.is_empty() {
        if migration_skipped.is_empty() {
            bail!(
                "error.bad_input(code=migrate_rust_type_usages_no_edits): no legal migration sites for `{}` in {}",
                module_name,
                source_path.display()
            );
        }
        let locations = join_locations_struct(&migration_skipped);
        bail!("error.bad_input(code=migrate_rust_type_usages_no_edits): {locations}");
    }

    let plan = RefactorPlan {
        title: format!(
            "migrate type usages of {module_name} in {}",
            path_string(&source_path)
        ),
        kind: "migrate_rust_type_usages".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: canonical_edits,
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

    validate_plan_shape(&plan).context("failed to validate migrate_rust_type_usages plan")?;
    let response = PlanWithMigrationSkipped {
        plan,
        migration_skipped,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

fn collect_type_sites<'a>(
    root: Node<'a>,
    source: &'a str,
    module_name: &str,
) -> (Vec<MigrationCandidate>, Vec<MigrationSkippedSite>) {
    let mut accepted = Vec::new();
    let mut skipped = Vec::new();

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // Catch both type-position `type_identifier` and value-position
        // `identifier` when it sits in the `path` slot of a `scoped_identifier`
        // / `scoped_type_identifier`. The latter is how `OldService::new()`,
        // `OldService::method()`, `OldService::CONST`, and turbofish forms
        // appear in tree-sitter-rust 0.24.
        let is_type_id = node.is_named() && node.kind() == "type_identifier";
        let is_value_path_head = node.is_named()
            && node.kind() == "identifier"
            && node
                .parent()
                .map(|p| {
                    matches!(p.kind(), "scoped_identifier" | "scoped_type_identifier")
                        && p.child_by_field_name("path")
                            .map(|path| path.start_byte() == node.start_byte())
                            .unwrap_or(false)
                })
                .unwrap_or(false);
        if is_type_id || is_value_path_head {
            let text = node.utf8_text(source.as_bytes()).unwrap_or("");
            if text == module_name {
                let (line, column) = line_col(source, node.start_byte());
                let candidate_position = infer_type_position(node);
                if is_typeid_reflection(node, source) {
                    skipped.push(MigrationSkippedSite {
                        line,
                        column,
                        reason: "typeid_reflection".to_string(),
                    });
                } else if is_associated_item_path(node, source) {
                    skipped.push(MigrationSkippedSite {
                        line,
                        column,
                        reason: "associated_item_or_ctor_path".to_string(),
                    });
                } else if is_turbofish(node) {
                    skipped.push(MigrationSkippedSite {
                        line,
                        column,
                        reason: "turbofish".to_string(),
                    });
                } else if is_pattern_position(node) {
                    skipped.push(MigrationSkippedSite {
                        line,
                        column,
                        reason: "pattern_position".to_string(),
                    });
                } else if matches!(candidate_position, TypeUsagePosition::Unsupported) {
                    skipped.push(MigrationSkippedSite {
                        line,
                        column,
                        reason: "unsupported_type_position".to_string(),
                    });
                } else {
                    let anchor = find_generic_anchor(node).map(|anchor_node| GenericAnchor {
                        start_byte: anchor_node.start_byte(),
                        end_byte: anchor_node.end_byte(),
                    });
                    accepted.push(MigrationCandidate {
                        node_start: node.start_byte(),
                        node_end: node.end_byte(),
                        line,
                        column,
                        position: candidate_position,
                        anchor,
                    });
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }

    (accepted, skipped)
}

fn infer_type_position(node: Node<'_>) -> TypeUsagePosition {
    if is_pattern_position(node) {
        TypeUsagePosition::Unsupported
    } else if is_in_field(node) {
        TypeUsagePosition::Field
    } else if is_in_function_param(node) {
        TypeUsagePosition::FunctionParam
    } else if is_in_function_return(node) {
        TypeUsagePosition::FunctionReturn
    } else if is_in_type_alias(node) {
        TypeUsagePosition::TypeAlias
    } else if is_in_generic_argument(node) {
        TypeUsagePosition::GenericArg
    } else if is_in_local_binding(node) {
        TypeUsagePosition::LocalBinding
    } else {
        TypeUsagePosition::Unsupported
    }
}

fn is_in_field(node: Node<'_>) -> bool {
    is_ancestor_with_field(node, "field_declaration", "type")
}

fn is_in_function_param(node: Node<'_>) -> bool {
    is_ancestor_with_field(node, "parameter", "type")
}

fn is_in_function_return(node: Node<'_>) -> bool {
    is_ancestor_with_field(node, "function_item", "return_type")
        || is_ancestor_with_field(node, "function_signature", "return_type")
}

fn is_in_type_alias(node: Node<'_>) -> bool {
    is_ancestor_with_field(node, "type_item", "type")
}

fn is_in_local_binding(node: Node<'_>) -> bool {
    is_ancestor_with_field(node, "let_declaration", "type")
        || is_ancestor_with_field(node, "const_item", "type")
        || is_ancestor_with_field(node, "static_item", "type")
}

fn is_in_generic_argument(node: Node<'_>) -> bool {
    if is_turbofish(node) {
        return false;
    }

    first_ancestor_kind(node, "generic_type")
        .is_some()
        || first_ancestor_kind(node, "generic_type_with_turbofish").is_some()
        || first_ancestor_kind(node, "generic_argument").is_some()
        || first_ancestor_kind(node, "type_arguments").is_some()
}

fn is_ancestor_with_field(node: Node<'_>, ancestor_kind: &str, field_name: &str) -> bool {
    let mut current = Some(node);
    while let Some(current_node) = current {
        if current_node.kind() == ancestor_kind {
            if let Some(field_node) = current_node.child_by_field_name(field_name) {
                if range_contains(field_node, node) {
                    return true;
                }
            }
        }
        current = current_node.parent();
    }
    false
}

fn first_ancestor_kind<'a>(node: Node<'a>, kind: &'a str) -> Option<Node<'a>> {
    let mut current = Some(node);
    while let Some(current_node) = current {
        if current_node.kind() == kind {
            return Some(current_node);
        }
        current = current_node.parent();
    }
    None
}

fn range_contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && outer.end_byte() >= inner.end_byte()
}

fn is_pattern_position(node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(current_node) = current {
        if current_node.kind().ends_with("_pattern") || current_node.kind() == "pattern" {
            return true;
        }
        current = current_node.parent();
    }
    false
}

fn is_associated_item_path(node: Node<'_>, _source: &str) -> bool {
    let mut current = Some(node);
    while let Some(current_node) = current {
        match current_node.kind() {
            "scoped_identifier" => {
                if let Some(path) = current_node.child_by_field_name("path") {
                    if range_contains(path, node) {
                        return true;
                    }
                }
            }
            "scoped_type_identifier" => {
                if let Some(path) = current_node.child_by_field_name("path") {
                    if range_contains(path, node) {
                        return true;
                    }
                }
                if let Some(type_name) = current_node.child_by_field_name("type") {
                    if range_contains(type_name, node) {
                        return true;
                    }
                }
            }
            _ => {}
        }
        current = current_node.parent();
    }
    false
}

fn is_turbofish(node: Node<'_>) -> bool {
    // Two shapes for fn-call turbofish in tree-sitter-rust 0.24:
    //   1. legacy `generic_type` under `call_expression`'s `function` field
    //   2. `generic_function { function, type_arguments }` under `call_expression`
    // Plus the directly-under-`type_arguments` case for both function-call and
    // explicit `<T as Trait>::method::<T>` turbofish forms.
    let mut current = Some(node);
    while let Some(current_node) = current {
        // Case (3): direct `type_arguments` ancestor whose parent is a callable shape.
        if current_node.kind() == "type_arguments" {
            if let Some(parent) = current_node.parent() {
                match parent.kind() {
                    "generic_function" | "scoped_identifier" | "call_expression" => return true,
                    _ => {}
                }
            }
        }
        // Case (2): generic_function node directly.
        if current_node.kind() == "generic_function" {
            return true;
        }
        // Case (1): legacy generic_type as call's function.
        if current_node.kind() == "generic_type"
            && let Some(parent_call) = current_node.parent()
            && parent_call.kind() == "call_expression"
            && let Some(function) = parent_call.child_by_field_name("function")
            && range_contains(function, current_node)
        {
            return true;
        }
        current = current_node.parent();
    }
    false
}

fn is_typeid_reflection(node: Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    while let Some(current_node) = current {
        if current_node.kind() == "call_expression" {
            if let Some(function) = current_node.child_by_field_name("function") {
                if range_contains(function, node) && is_typeid_of_function(function, source) {
                    return true;
                }
            }
        }
        current = current_node.parent();
    }
    false
}

fn is_typeid_of_function(function: Node<'_>, source: &str) -> bool {
    // For `TypeId::of::<X>()` the function field is a `generic_function`
    // wrapping a `scoped_identifier`. Strip the turbofish (`type_arguments`)
    // suffix by anchoring on the inner scoped path.
    let inner = if function.kind() == "generic_function" {
        function.child_by_field_name("function").unwrap_or(function)
    } else {
        function
    };
    let normalized = inner
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .replace(' ', "");
    normalized == "TypeId::of" || normalized.ends_with("::TypeId::of")
}

fn find_generic_anchor(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(current_node) = current {
        match current_node.kind() {
            "function_item" | "impl_item" | "struct_item" | "enum_item" | "trait_item"
            | "type_item" => return Some(current_node),
            _ => {}
        }
        current = current_node.parent();
    }
    None
}

fn find_node_by_range(node: Node<'_>, start: usize, end: usize) -> Option<Node<'_>> {
    if node.start_byte() == start && node.end_byte() == end {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > end || child.end_byte() < start {
            continue;
        }
        if let Some(found) = find_node_by_range(child, start, end) {
            return Some(found);
        }
    }
    None
}

fn collect_line_cols_for_anchor_range(
    sites: &[MigrationCandidate],
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    sites
        .iter()
        .filter_map(|site| {
            site.anchor
                .as_ref()
                .and_then(|anchor| {
                    (anchor.start_byte == start && anchor.end_byte == end)
                        .then_some((site.line, site.column))
                })
        })
        .collect()
}

fn has_generic_type_param_t(item: &Node<'_>, source: &str) -> bool {
    let Some(tp_node) = item.child_by_field_name("type_parameters") else {
        return false;
    };
    has_t_parameter_name(tp_node, source)
}

fn has_t_parameter_name(node: Node<'_>, source: &str) -> bool {
    if matches!(node.kind(), "type_identifier" | "type_parameter" | "identifier") {
        if let Ok(name) = node.utf8_text(source.as_bytes()) {
            if name == "T" {
                return true;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if has_t_parameter_name(child, source) {
            return true;
        }
    }
    false
}

fn build_add_generic_param_edit(item: &Node<'_>, source: &str) -> Result<TextEdit> {
    if let Some(tp_node) = item.child_by_field_name("type_parameters") {
        let insert_at = tp_node.end_byte().saturating_sub(1);
        let inner = &source[tp_node.start_byte().saturating_add(1)..tp_node.end_byte().saturating_sub(1)]
            .trim();
        let replacement = if inner.is_empty() { "T" } else { ", T" };
        return Ok(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement: replacement.to_string(),
        });
    }

    let insert_at = if item.kind() == "impl_item" {
        insert_after_keyword(item, source, "impl")?
    } else if let Some(name_node) = item.child_by_field_name("name") {
        name_node.end_byte()
    } else {
        find_name_end(item, source)
    };

    Ok(TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement: "<T>".to_string(),
    })
}

fn insert_after_keyword(item: &Node<'_>, source: &str, keyword: &str) -> Result<usize> {
    let slice = source
        .get(item.start_byte()..item.end_byte())
        .context("invalid item range")?;
    let start = slice
        .find(keyword)
        .with_context(|| format!("failed to locate `{keyword}` token in `{}`", item.kind()))?;
    let start = item.start_byte() + start + keyword.len();
    let mut i = start;
    while i < item.end_byte() && source.as_bytes()[i].is_ascii_whitespace() {
        i += 1;
    }
    Ok(i)
}

fn find_name_end(item: &Node<'_>, source: &str) -> usize {
    let start = item.start_byte();
    let mut end = item.start_byte();
    let mut i = item.start_byte();
    while i < item.end_byte() {
        if source.as_bytes()[i].is_ascii_whitespace()
            || source.as_bytes()[i] == b'{'
            || source.as_bytes()[i] == b'='
            || source.as_bytes()[i] == b';'
            || source.as_bytes()[i] == b':'
            || source.as_bytes()[i] == b'>'
        {
            if i > start {
                end = i;
                break;
            }
        }
        i += 1;
    }

    if end == item.start_byte() {
        return item.start_byte();
    }
    end
}

fn build_where_clause_edit(item: &Node<'_>, source: &str, bound_text: &str) -> Option<TextEdit> {
    if let Some(where_node) = item.child_by_field_name("where_clause") {
        let mut insert_at = where_node.end_byte();
        while insert_at > where_node.start_byte() && source.as_bytes()[insert_at - 1].is_ascii_whitespace() {
            insert_at -= 1;
        }
        Some(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement: format!(", T: {bound_text}"),
        })
    } else if let Some(header_end) = end_of_item_header(item, source) {
        Some(TextEdit {
            byte_start: header_end,
            byte_end: header_end,
            replacement: format!(" where T: {bound_text}"),
        })
    } else {
        None
    }
}

fn end_of_item_header(item: &Node<'_>, source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = item.start_byte();
    while i < item.end_byte() {
        if bytes[i] == b'{' || bytes[i] == b'=' || bytes[i] == b';' {
            // Walk back over trailing whitespace so the inserted ` where ...`
            // doesn't double-space against the header.
            let mut insert_at = i;
            while insert_at > item.start_byte() && bytes[insert_at - 1].is_ascii_whitespace() {
                insert_at -= 1;
            }
            return Some(insert_at);
        }
        i += 1;
    }
    None
}

fn canonicalize_and_merge(mut edits: Vec<TextEdit>) -> Result<Vec<TextEdit>> {
    edits.sort_by_key(|edit| (edit.byte_start, edit.byte_end));
    edits.dedup_by(|a, b| {
        a.byte_start == b.byte_start && a.byte_end == b.byte_end && a.replacement == b.replacement
    });
    Ok(edits)
}

fn summarize_sites(sites: &[MigrationCandidate]) -> String {
    sites
        .iter()
        .map(|site| format!("{}:{}", site.line, site.column))
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_locations_struct(sites: &[MigrationSkippedSite]) -> String {
    sites
        .iter()
        .map(|site| format!("{}:{}", site.line, site.column))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    fn parse_response(plan_json: &str) -> (Vec<TextEdit>, Vec<MigrationSkippedSite>) {
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
        let skipped: Vec<MigrationSkippedSite> = value
            .get("migration_skipped")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|v| MigrationSkippedSite {
                        line: v["line"].as_u64().unwrap_or(0) as usize,
                        column: v["column"].as_u64().unwrap_or(0) as usize,
                        reason: v["reason"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        (edits, skipped)
    }

    fn plan_for(
        replacement_kind: &str,
        source: &Path,
        new_text: &str,
    ) -> (Vec<TextEdit>, Vec<MigrationSkippedSite>) {
        let response = plan_migrate_type_usages(&crate::refactor::RefactorPlanParams {
            kind: "migrate_rust_type_usages".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("OldService".to_string()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some(replacement_kind.to_string()),
            new_text: Some(new_text.to_string()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            callback_externals: None,
            output_path: None,
        })
        .unwrap();
        parse_response(&response)
    }

    #[test]
    fn rust_migrate_type_usages_bare_concrete_covers_supported_type_positions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
struct Holder {
    service: OldService,
}

fn consume(value: OldService, list: Vec<OldService>) -> OldService {
    let tmp: OldService = value;
    let ptr: Vec<OldService> = Vec::new();
    type Alias = OldService;
    tmp
}
"#,
        )
        .unwrap();

        let (edits, skipped) = plan_for("BareConcrete", &source, "Service");
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");
        assert_eq!(edits.len(), 7);
        assert!(edits.iter().all(|edit| edit.replacement == "Service"));
    }

    #[test]
    fn rust_migrate_type_usages_wrapper_kinds_apply_in_their_supported_positions() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
struct Holder {
    service: OldService,
}

fn consume(value: OldService, list: Vec<OldService>) -> OldService {
    type Alias = OldService;
    value
}
"#,
        )
        .unwrap();

        for (kind, replacement) in [
            ("BoxDyn", "Box<dyn Service>"),
            ("ArcDyn", "Arc<dyn Service>"),
            ("RcDyn", "Rc<dyn Service>"),
        ] {
            let (edits, skipped) = plan_for(kind, &source, replacement);
            assert_eq!(skipped.len(), 0, "{kind} should not skip legal positions");
            assert!(
                !edits.is_empty(),
                "{kind} should produce edits"
            );
            assert!(
                edits.iter().all(|edit| edit.replacement == replacement),
                "{kind} replacement mismatch"
            );
        }
    }

    #[test]
    fn rust_migrate_type_usages_impl_trait_only_fn_param_and_fn_return() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
struct Holder {
    service: OldService,
}

fn consume(input: OldService, output: Vec<OldService>) -> OldService {
    input
}
"#,
        )
        .unwrap();

        let err = plan_migrate_type_usages(&crate::refactor::RefactorPlanParams {
            kind: "migrate_rust_type_usages".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("OldService".to_string()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some("ImplTrait".to_string()),
            new_text: Some("impl Service".to_string()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            callback_externals: None,
            output_path: None,
        });
        let msg = err.expect_err("impl trait should fail").to_string();
        assert!(
            msg.contains("error.bad_input(code=impl_trait_illegal_position)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn rust_migrate_type_usages_generic_param_t_bounded_trait_adds_generics_and_where() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn consume(value: OldService, list: Vec<OldService>) -> Option<OldService> {
    consume_inner(value)
}

fn consume_inner(value: Vec<OldService>) -> Option<OldService> {
    None
}
"#,
        )
        .unwrap();

        let (edits, skipped) = plan_for("GenericParamTBoundedTrait", &source, "Service");
        assert!(skipped.is_empty());
        let source_text = std::fs::read_to_string(&source).unwrap();
        let rewritten = super::apply_text_edits(&source_text, &edits).unwrap();
        assert!(rewritten.contains("fn consume<T>(value: T, list: Vec<T>) -> Option<T>"));
        assert!(rewritten.contains("fn consume_inner<T>(value: Vec<T>) -> Option<T>"));
        assert!(rewritten.contains("where T: Service"));
    }

    #[test]
    fn rust_migrate_type_usages_generic_param_t_bounded_trait_conflict_raises_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn consume<T>(value: OldService, list: Vec<OldService>) {
    let _ = value;
}
"#,
        )
        .unwrap();

        let err = plan_migrate_type_usages(&crate::refactor::RefactorPlanParams {
            kind: "migrate_rust_type_usages".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("OldService".to_string()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some("GenericParamTBoundedTrait".to_string()),
            new_text: Some("Service".to_string()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            callback_externals: None,
            output_path: None,
        });
        let msg = err.expect_err("expected conflict").to_string();
        assert!(
            msg.contains("error.bad_input(code=generic_param_conflict)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn rust_migrate_type_usages_skips_forbidden_categories() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn test(value: OldService) {
    let _ = OldService::new();
    let _ = OldService::method();
    let _ = OldService::CONST;
    let _ = helper::<OldService>();
    let _ = TypeId::of::<OldService>();
    if let OldService {} = value {}
}
"#,
        )
        .unwrap();

        let (edits, skipped) = plan_for("BareConcrete", &source, "Service");
        assert_eq!(edits.len(), 1);
        let reasons: HashSet<&str> = skipped.iter().map(|site| site.reason.as_str()).collect();
        assert!(reasons.contains("associated_item_or_ctor_path"));
        assert!(reasons.contains("turbofish"));
        assert!(reasons.contains("typeid_reflection"));
        assert!(reasons.contains("pattern_position"));
    }

    #[test]
    fn rust_migrate_type_usages_impl_trait_refuses_struct_field() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
struct Holder {
    service: OldService,
}
"#,
        )
        .unwrap();

        let err = plan_migrate_type_usages(&crate::refactor::RefactorPlanParams {
            kind: "migrate_rust_type_usages".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("OldService".to_string()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some("ImplTrait".to_string()),
            new_text: Some("impl Service".to_string()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            callback_externals: None,
            output_path: None,
        });
        let msg = err.expect_err("impl trait should refuse struct field").to_string();
        assert!(
            msg.contains("error.bad_input(code=impl_trait_illegal_position)"),
            "message was: {msg}"
        );
    }

    #[test]
    fn rust_migrate_type_usages_generic_param_t_bounded_trait_handles_functions_without_where() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        std::fs::write(
            &source,
            r#"
fn consume(v: OldService) -> OldService {
    v
}
"#,
        )
        .unwrap();

        let (edits, skipped) = plan_for("GenericParamTBoundedTrait", &source, "Service");
        assert!(skipped.is_empty());
        let source_text = std::fs::read_to_string(&source).unwrap();
        let rewritten = super::apply_text_edits(&source_text, &edits).unwrap();
        assert!(
            rewritten.contains("fn consume<T>(v: T) -> T where T: Service"),
            "rewritten output: {rewritten}\nedits: {edits:?}"
        );
    }
}

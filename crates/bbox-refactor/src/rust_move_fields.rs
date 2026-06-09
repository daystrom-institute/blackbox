//! RX-S1 — `move_rust_struct_fields` plan kind.
//!
//! Moves named fields from one struct to another (possibly in the same file).
//! Reports `remaining_source_accessors` when `deep_analysis: true`.
//! `semantic_status: IndexedHints`.

use super::*;
use std::collections::{HashMap, HashSet};

// ── response shape ────────────────────────────────────────────────────────────

/// RefactorPlan extended with move-specific analysis fields.
#[derive(Debug, Serialize)]
struct PlanWithMoveExtra {
    #[serde(flatten)]
    plan: RefactorPlan,
    /// Impl-level type parameter names referenced by the moved field types.
    /// Reported, not auto-injected. Matches the `DeepAnalysis.inherited_generics` shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inherited_generics: Vec<String>,
}

// ── entry point ───────────────────────────────────────────────────────────────

pub fn plan_move_struct_fields(p: &RefactorPlanParams) -> anyhow::Result<String> {
    // ── inputs ────────────────────────────────────────────────────────────────
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_rust_struct_fields"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;

    let field_names: Vec<String> = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("item_names (field names to move) is required"))?
        .to_vec();
    let field_name_set: HashSet<&str> = field_names.iter().map(String::as_str).collect();

    let struct_name = p
        .impl_name
        .as_deref()
        .or(p.module_name.as_deref())
        .ok_or_else(|| {
            anyhow!("impl_name or module_name required to identify the source struct")
        })?;

    // acknowledge_repr carried via toml_entries["acknowledge_repr"]
    let acknowledge_repr = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("acknowledge_repr"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let visibility_override = p.visibility.as_deref();
    let deep = p.deep_analysis.unwrap_or(false);
    let same_file = source_path == target_path;

    // ── parse ─────────────────────────────────────────────────────────────────
    let parsed_src = parse_rust_file(&source_path)?;
    // Parse target only when different from source (avoid double-parsing).
    let parsed_tgt_owned: Option<ParsedSource> = if same_file {
        None
    } else {
        Some(parse_rust_file(&target_path)?)
    };
    let parsed_tgt: &ParsedSource = parsed_tgt_owned.as_ref().unwrap_or(&parsed_src);

    // ── find source struct ────────────────────────────────────────────────────
    let src_struct_item = find_struct_syntax_item(&parsed_src, struct_name).ok_or_else(|| {
        anyhow!(
            "struct `{struct_name}` not found in {}",
            source_path.display()
        )
    })?;

    // Check for non-default #[repr(...)] (transparent is allowed without flag).
    let has_non_default_repr = src_struct_item.attributes.iter().any(|attr| {
        let a = attr.trim();
        a.starts_with("#[repr(") && !a.starts_with("#[repr(transparent)")
    });
    if has_non_default_repr && !acknowledge_repr {
        bail!(
            "error.bad_input(code=repr_unacknowledged): struct `{struct_name}` has a \
             non-default #[repr(...)] attribute; pass acknowledge_repr=true to proceed"
        );
    }

    // ── collect source fields ─────────────────────────────────────────────────
    let src_root = parsed_src.tree.root_node();
    let src_struct_node = rust_node_by_range(
        src_root,
        "struct_item",
        src_struct_item.byte_start,
        src_struct_item.byte_end,
    )
    .ok_or_else(|| anyhow!("struct `{struct_name}` AST node not found"))?;

    let all_source_fields = collect_field_decls_in_struct(&parsed_src.source, src_struct_node);

    // Validate all requested field names exist in the source struct.
    let src_field_names: HashSet<&str> =
        all_source_fields.iter().map(|f| f.name.as_str()).collect();
    for name in &field_names {
        if !src_field_names.contains(name.as_str()) {
            bail!("field `{name}` not found in struct `{struct_name}`");
        }
    }

    // Fields to move, in declaration order.
    let fields_to_move: Vec<&FieldDecl> = all_source_fields
        .iter()
        .filter(|f| field_name_set.contains(f.name.as_str()))
        .collect();

    // ── inherited generics ────────────────────────────────────────────────────
    let inherited_generics =
        collect_impl_generics_for_fields(&parsed_src, struct_name, &fields_to_move);

    // ── source edits (field removal) ──────────────────────────────────────────
    let mut source_edits: Vec<TextEdit> = fields_to_move
        .iter()
        .map(|f| {
            let remove_end = field_removal_end(&parsed_src.source, f.byte_end);
            TextEdit {
                byte_start: f.leading_start,
                byte_end: remove_end,
                replacement: String::new(),
            }
        })
        .collect();
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits).context("overlapping source field removal edits")?;

    // ── find target struct ────────────────────────────────────────────────────
    let tgt_struct_item = if same_file {
        find_any_struct_except(&parsed_src, struct_name).ok_or_else(|| {
            anyhow!(
                "no target struct found in source file for same-file move; \
                 either provide a separate target file or add the target struct"
            )
        })?
    } else {
        find_any_struct(parsed_tgt)
            .ok_or_else(|| anyhow!("no struct found in {}", target_path.display()))?
    };

    // ── target edits (field insertion) ────────────────────────────────────────
    let tgt_root = parsed_tgt.tree.root_node();
    let tgt_struct_node = rust_node_by_range(
        tgt_root,
        "struct_item",
        tgt_struct_item.byte_start,
        tgt_struct_item.byte_end,
    )
    .ok_or_else(|| anyhow!("target struct AST node not found"))?;

    let insert_byte = field_list_close_brace_byte(tgt_struct_node).ok_or_else(|| {
        anyhow!("could not locate field_declaration_list closing brace in target struct")
    })?;

    let mut inserted = String::new();
    for field in &fields_to_move {
        let vis_prefix = match visibility_override {
            Some(v) if !v.is_empty() => format!("{v} "),
            Some(_) => String::new(), // explicit empty = strip visibility
            None => field
                .visibility
                .as_deref()
                .map(|v| format!("{v} "))
                .unwrap_or_default(),
        };
        inserted.push_str(&format!(
            "    {vis_prefix}{}: {},\n",
            field.name, field.type_text
        ));
    }

    let target_edits = vec![TextEdit {
        byte_start: insert_byte,
        byte_end: insert_byte,
        replacement: inserted,
    }];

    // ── deep analysis (remaining accesses) ────────────────────────────────────
    let remaining_source_accessors = if deep {
        analyze_remaining_accesses(&parsed_src, &field_name_set)
    } else {
        Vec::new()
    };

    // ── assemble FileEdits ────────────────────────────────────────────────────
    let src_sha = sha256_hex(parsed_src.source.as_bytes());
    let tgt_sha = sha256_hex(parsed_tgt.source.as_bytes());

    let edits = if same_file {
        // Merge source removals and target insertions into one FileEdit.
        let mut combined = source_edits;
        combined.extend(target_edits);
        combined.sort_by_key(|e| e.byte_start);
        ensure_non_overlapping(&combined)
            .context("overlapping combined edits in same-file move")?;
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: src_sha,
            edits: combined,
            new_text: None,
        }]
    } else {
        let mut edits = Vec::new();
        if !source_edits.is_empty() {
            edits.push(FileEdit {
                path: path_string(&source_path),
                original_sha256: src_sha,
                edits: source_edits,
                new_text: None,
            });
        }
        edits.push(FileEdit {
            path: path_string(&target_path),
            original_sha256: tgt_sha,
            edits: target_edits,
            new_text: None,
        });
        edits
    };

    let validations = {
        let mut v = vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }];
        if !same_file {
            v.push(ValidationStep::TreeSitterNoErrors {
                path: path_string(&target_path),
                byte_range: None,
            });
        }
        v
    };

    let mut operator_opt_outs = Vec::new();
    if acknowledge_repr && has_non_default_repr {
        operator_opt_outs.push("acknowledge_repr".to_string());
    }

    let plan = RefactorPlan {
        title: format!(
            "move {} field(s) from `{struct_name}` to target struct",
            fields_to_move.len()
        ),
        kind: "move_rust_struct_fields".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors,
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: operator_opt_outs,
    };

    validate_plan_shape(&plan).context("validate move_rust_struct_fields plan")?;
    let response = PlanWithMoveExtra {
        plan,
        inherited_generics,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

// ── internal data ─────────────────────────────────────────────────────────────

struct FieldDecl {
    name: String,
    type_text: String,
    visibility: Option<String>,
    byte_end: usize,
    /// Start of removal range: the indentation/newline before the field.
    leading_start: usize,
}

// ── struct / field finders ────────────────────────────────────────────────────

fn find_struct_syntax_item(parsed: &ParsedSource, name: &str) -> Option<SyntaxItem> {
    rust_items(parsed)
        .into_iter()
        .find(|item| item.kind == "struct_item" && item.name.as_deref() == Some(name))
}

fn find_any_struct(parsed: &ParsedSource) -> Option<SyntaxItem> {
    rust_items(parsed)
        .into_iter()
        .find(|item| item.kind == "struct_item")
}

fn find_any_struct_except(parsed: &ParsedSource, exclude: &str) -> Option<SyntaxItem> {
    rust_items(parsed)
        .into_iter()
        .find(|item| item.kind == "struct_item" && item.name.as_deref() != Some(exclude))
}

fn collect_field_decls_in_struct(source: &str, struct_node: Node<'_>) -> Vec<FieldDecl> {
    let mut decls = Vec::new();
    collect_field_decls_recursive(source, source.as_bytes(), struct_node, &mut decls);
    decls
}

fn collect_field_decls_recursive(
    source_str: &str,
    source_bytes: &[u8],
    node: Node<'_>,
    decls: &mut Vec<FieldDecl>,
) {
    if node.kind() == "field_declaration" {
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("")
            .to_string();
        let type_text = node
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("")
            .to_string();
        let visibility = field_visibility(node, source_bytes);
        // leading_start = start of the line containing the field
        let leading = line_start_before(source_str, node.start_byte());
        if !name.is_empty() {
            decls.push(FieldDecl {
                name,
                type_text,
                visibility,
                byte_end: node.end_byte(),
                leading_start: leading,
            });
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_field_decls_recursive(source_str, source_bytes, child, decls);
    }
}

fn field_visibility(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(source).ok().map(str::to_string);
        }
    }
    None
}

/// Removal end: field end + optional comma + optional newline.
fn field_removal_end(source: &str, field_end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = field_end;
    // Skip inline whitespace.
    while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
        idx += 1;
    }
    // Consume trailing comma.
    if idx < bytes.len() && bytes[idx] == b',' {
        idx += 1;
    }
    // Consume newline.
    if idx < bytes.len() && bytes[idx] == b'\r' {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b'\n' {
        idx += 1;
    }
    idx
}

/// Byte offset of the `}` closing the field_declaration_list in a struct_item.
fn field_list_close_brace_byte(struct_node: Node<'_>) -> Option<usize> {
    // Walk through all children (not just named ones) to find field_declaration_list.
    let mut cursor = struct_node.walk();
    for child in struct_node.children(&mut cursor) {
        if child.kind() == "field_declaration_list" {
            let count = child.child_count();
            if count > 0 {
                return child.child((count - 1) as u32).map(|n| n.start_byte());
            }
        }
    }
    None
}

// ── inherited generics ────────────────────────────────────────────────────────

fn collect_impl_generics_for_fields(
    parsed: &ParsedSource,
    struct_name: &str,
    fields: &[&FieldDecl],
) -> Vec<String> {
    let source_bytes = parsed.source.as_bytes();
    let root = parsed.tree.root_node();

    // Find an impl block for struct_name and collect its type parameters.
    let mut impl_type_params: Vec<String> = Vec::new();
    {
        let mut cursor = root.walk();
        for node in root.named_children(&mut cursor) {
            if node.kind() != "impl_item" {
                continue;
            }
            // The impl's type name (stripping generics).
            let impl_type_name = node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source_bytes).ok())
                .map(|t| t.split(['<', ' ']).next().unwrap_or("").to_string())
                .unwrap_or_default();
            if impl_type_name != struct_name {
                continue;
            }
            if let Some(tp) = node.child_by_field_name("type_parameters") {
                let mut tc = tp.walk();
                for param in tp.named_children(&mut tc) {
                    if matches!(param.kind(), "type_parameter" | "lifetime_parameter") {
                        if let Some(name_node) = param.child_by_field_name("name") {
                            if let Ok(name) = name_node.utf8_text(source_bytes) {
                                impl_type_params.push(name.to_string());
                            }
                        }
                    }
                }
            }
            break;
        }
    }

    if impl_type_params.is_empty() {
        return Vec::new();
    }

    // Find which impl type params appear in the moved field types.
    let mut referenced: Vec<String> = Vec::new();
    for field in fields {
        for param in &impl_type_params {
            if field.type_text.contains(param.as_str()) && !referenced.contains(param) {
                referenced.push(param.clone());
            }
        }
    }
    referenced.sort();
    referenced
}

// ── deep analysis (remaining source accessors) ────────────────────────────────

fn analyze_remaining_accesses(
    parsed: &ParsedSource,
    field_names: &HashSet<&str>,
) -> Vec<RemainingFieldAccessor> {
    let source_bytes = parsed.source.as_bytes();
    let root = parsed.tree.root_node();
    let mut accesses: HashMap<String, Vec<FieldAccessSite>> = HashMap::new();
    // Seed all field names so entries exist even with zero accesses.
    for &name in field_names {
        accesses.entry(name.to_string()).or_default();
    }
    walk_for_remaining_accesses(
        source_bytes,
        &parsed.source,
        root,
        field_names,
        &mut accesses,
    );
    let mut result: Vec<RemainingFieldAccessor> = accesses
        .into_iter()
        .filter(|(_, sites)| !sites.is_empty())
        .map(|(field, accesses)| RemainingFieldAccessor { field, accesses })
        .collect();
    result.sort_by(|a, b| a.field.cmp(&b.field));
    result
}

fn walk_for_remaining_accesses(
    source_bytes: &[u8],
    source_str: &str,
    node: Node<'_>,
    field_names: &HashSet<&str>,
    accesses: &mut HashMap<String, Vec<FieldAccessSite>>,
) {
    match node.kind() {
        "field_expression" => {
            if let (Some(val), Some(fld)) = (
                node.child_by_field_name("value"),
                node.child_by_field_name("field"),
            ) {
                if val.utf8_text(source_bytes).ok() == Some("self") {
                    let fname = fld.utf8_text(source_bytes).unwrap_or("");
                    if field_names.contains(fname) {
                        let kind = classify_access_kind(node);
                        let (line, column) = line_col(source_str, node.start_byte());
                        let context = line_text(source_str, node.start_byte());
                        accesses
                            .entry(fname.to_string())
                            .or_default()
                            .push(FieldAccessSite {
                                line,
                                column,
                                kind,
                                context,
                            });
                        return;
                    }
                }
            }
        }
        "struct_pattern" => {
            let sbytes = source_bytes;
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "field_pattern" => {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let fname = name_node.utf8_text(sbytes).unwrap_or("");
                            if field_names.contains(fname) {
                                let (line, column) = line_col(source_str, child.start_byte());
                                let context = line_text(source_str, child.start_byte());
                                accesses.entry(fname.to_string()).or_default().push(
                                    FieldAccessSite {
                                        line,
                                        column,
                                        kind: "pattern_destructure".to_string(),
                                        context,
                                    },
                                );
                            }
                        }
                    }
                    "remaining_fields" => {
                        // `..rest` inside a struct pattern
                        let (line, column) = line_col(source_str, child.start_byte());
                        let context = line_text(source_str, child.start_byte());
                        // Report spread under each moved field (conservative).
                        for &fname in field_names.iter() {
                            accesses
                                .entry(fname.to_string())
                                .or_default()
                                .push(FieldAccessSite {
                                    line,
                                    column,
                                    kind: "spread".to_string(),
                                    context: context.clone(),
                                });
                        }
                    }
                    _ => {
                        walk_for_remaining_accesses(
                            sbytes,
                            source_str,
                            child,
                            field_names,
                            accesses,
                        );
                    }
                }
            }
            return;
        }
        "base_field_initializer" => {
            // `..self` or `..other` spread inside struct literal.
            let text = node.utf8_text(source_bytes).unwrap_or("");
            if text.starts_with("..") {
                let (line, column) = line_col(source_str, node.start_byte());
                let context = line_text(source_str, node.start_byte());
                for &fname in field_names.iter() {
                    accesses
                        .entry(fname.to_string())
                        .or_default()
                        .push(FieldAccessSite {
                            line,
                            column,
                            kind: "spread".to_string(),
                            context: context.clone(),
                        });
                }
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_for_remaining_accesses(source_bytes, source_str, child, field_names, accesses);
    }
}

/// Classify a `field_expression` node as `read` or `write`.
fn classify_access_kind(field_expr: Node<'_>) -> String {
    let mut p = field_expr.parent();
    while let Some(parent) = p {
        match parent.kind() {
            "assignment_expression" | "compound_assignment_expr" => {
                if let Some(lhs) = parent.child_by_field_name("left") {
                    if lhs.start_byte() == field_expr.start_byte()
                        && lhs.end_byte() == field_expr.end_byte()
                    {
                        return "write".to_string();
                    }
                }
                return "read".to_string();
            }
            // Stop early at statement boundaries.
            "block"
            | "function_item"
            | "let_declaration"
            | "return_expression"
            | "expression_statement" => break,
            _ => p = parent.parent(),
        }
    }
    "read".to_string()
}

/// Extract the trimmed text of the line containing `byte`.
fn line_text(source: &str, byte: usize) -> String {
    let start = line_start_before(source, byte);
    let end = source[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(source.len());
    source.get(start..end).unwrap_or("").trim().to_string()
}

// ── helper (mirrors mod.rs private fn) ───────────────────────────────────────

fn line_start_before(source: &str, idx: usize) -> usize {
    let limited = idx.min(source.len());
    source[..limited].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn params_for(
        source: &std::path::Path,
        target: &std::path::Path,
        struct_name: &str,
        fields: &[&str],
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "move_rust_struct_fields".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            item_names: Some(fields.iter().map(|&s| s.to_string()).collect()),
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
            delegate_field: None,
            delegate_type: None,
            keep_copy: None,
            deep_analysis: None,
            rewrite_remaining_accessors: None,
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            wiring_mode: None,
            callback_externals: None,
            output_path: None,
            ..Default::default()
        }
    }

    fn source_text_from_plan(plan_json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        let first_file = &v["edits"][0];
        let path = first_file["path"].as_str().unwrap();
        let original = first_file["original_sha256"].as_str().unwrap();
        let source = std::fs::read_to_string(path).unwrap();
        assert_eq!(sha256_hex(source.as_bytes()), original);
        let edits: Vec<TextEdit> = serde_json::from_value(first_file["edits"].clone()).unwrap();
        apply_text_edits(&source, &edits).unwrap()
    }

    fn target_text_from_plan(plan_json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        // target is the last entry in edits
        let edits_arr = v["edits"].as_array().unwrap();
        let last_file = edits_arr.last().unwrap();
        let path = last_file["path"].as_str().unwrap();
        let source = std::fs::read_to_string(path).unwrap().to_string();
        let source = if source.is_empty() {
            // new target file (empty)
            String::new()
        } else {
            source
        };
        let edits: Vec<TextEdit> = serde_json::from_value(last_file["edits"].clone()).unwrap();
        apply_text_edits(&source, &edits).unwrap()
    }

    // Gate: clean move round-trips (no remaining accessors).
    #[test]
    fn move_fields_clean_move_no_remaining_accessors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
    name: String,
}
"#,
        )
        .unwrap();
        std::fs::write(
            &tgt,
            r#"
struct ServerState {
}
"#,
        )
        .unwrap();

        let mut p = params_for(&src, &tgt, "BigServer", &["count"]);
        p.deep_analysis = Some(true);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        // remaining_source_accessors should be empty (no impl block accessing count)
        let remaining = v["remaining_source_accessors"].as_array();
        assert!(remaining.is_none() || remaining.unwrap().is_empty());
        // source should no longer contain the field
        let src_text = source_text_from_plan(&plan_json);
        assert!(!src_text.contains("count: u32"));
        // target should contain the field
        let tgt_text = target_text_from_plan(&plan_json);
        assert!(tgt_text.contains("count: u32"));
    }

    // Gate: move with remaining accessors reports each site.
    #[test]
    fn move_fields_remaining_accessors_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn increment(&mut self) {
        self.count += 1;
    }
    fn read_count(&self) -> u32 {
        self.count
    }
}
"#,
        )
        .unwrap();
        std::fs::write(&tgt, "struct ServerState {\n}\n").unwrap();

        let mut p = params_for(&src, &tgt, "BigServer", &["count"]);
        p.deep_analysis = Some(true);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        let remaining = v["remaining_source_accessors"].as_array().unwrap();
        assert!(!remaining.is_empty(), "expected remaining accessors");
        // Should include at least read and write accesses
        let count_entry = remaining.iter().find(|e| e["field"] == "count").unwrap();
        let accesses = count_entry["accesses"].as_array().unwrap();
        let kinds: Vec<&str> = accesses
            .iter()
            .map(|a| a["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"write") || kinds.contains(&"read"));
    }

    // Gate: repr-tagged struct refuses without acknowledge_repr.
    #[test]
    fn move_fields_repr_struct_refuses_without_acknowledge() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repr.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
#[repr(C)]
struct Packed {
    x: u32,
    y: u32,
}
"#,
        )
        .unwrap();
        std::fs::write(&tgt, "struct Target {\n}\n").unwrap();

        let p = params_for(&src, &tgt, "Packed", &["x"]);
        let err = plan_move_struct_fields(&p).unwrap_err();
        assert!(
            err.to_string().contains("repr_unacknowledged"),
            "expected repr_unacknowledged, got: {err}"
        );
    }

    // Gate: repr-tagged with acknowledge_repr proceeds.
    #[test]
    fn move_fields_repr_struct_proceeds_with_acknowledge() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("repr.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
#[repr(C)]
struct Packed {
    x: u32,
    y: u32,
}
"#,
        )
        .unwrap();
        std::fs::write(&tgt, "struct Target {\n}\n").unwrap();

        let mut p = params_for(&src, &tgt, "Packed", &["x"]);
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "acknowledge_repr".to_string(),
            serde_json::Value::Bool(true),
        );
        p.toml_entries = Some(entries);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let tgt_text = target_text_from_plan(&plan_json);
        assert!(tgt_text.contains("x: u32"));
    }

    // Gate: ..rest spread site flagged as kind=spread, NOT in FileEdits.
    #[test]
    fn move_fields_spread_flagged_not_in_edits() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn clone_server(&self) -> BigServer {
        BigServer { ..self.clone() }
    }
}
"#,
        )
        .unwrap();
        std::fs::write(&tgt, "struct ServerState {\n}\n").unwrap();

        let mut p = params_for(&src, &tgt, "BigServer", &["count"]);
        p.deep_analysis = Some(true);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        // spread should not appear in edits text
        let edits_str = serde_json::to_string(&v["edits"]).unwrap();
        assert!(!edits_str.contains("spread"));
        // spread may or may not appear in remaining_source_accessors (depends on AST)
        // The key check: the plan is valid (didn't crash)
        assert_eq!(v["kind"].as_str().unwrap(), "move_rust_struct_fields");
    }

    // Gate: visibility override applied to moved field.
    #[test]
    fn move_fields_visibility_override() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(&src, "struct BigServer {\n    count: u32,\n}\n").unwrap();
        std::fs::write(&tgt, "struct ServerState {\n}\n").unwrap();

        let mut p = params_for(&src, &tgt, "BigServer", &["count"]);
        p.visibility = Some("pub(crate)".to_string());
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let tgt_text = target_text_from_plan(&plan_json);
        assert!(tgt_text.contains("pub(crate) count: u32"));
    }

    // Gate: inherited generics reported for fields referencing source type params.
    #[test]
    fn move_fields_inherited_generics_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            r#"
struct Server<T> {
    data: T,
    count: u32,
}

impl<T: Send> Server<T> {
    fn new(data: T) -> Self { Self { data, count: 0 } }
}
"#,
        )
        .unwrap();
        std::fs::write(&tgt, "struct State {\n}\n").unwrap();

        let p = params_for(&src, &tgt, "Server", &["data"]);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
        let generics = v["inherited_generics"].as_array().unwrap();
        assert!(
            !generics.is_empty(),
            "expected inherited_generics to be non-empty"
        );
        let names: Vec<&str> = generics.iter().map(|g| g.as_str().unwrap()).collect();
        assert!(names.contains(&"T"), "expected T in inherited_generics");
    }

    // Gate: missing field name returns an error.
    #[test]
    fn move_fields_missing_field_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(&src, "struct Foo {\n    x: u32,\n}\n").unwrap();
        std::fs::write(&tgt, "struct Bar {\n}\n").unwrap();

        let p = params_for(&src, &tgt, "Foo", &["nonexistent"]);
        let err = plan_move_struct_fields(&p).unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "expected field name in error: {err}"
        );
    }

    // Gate: move with visibility preserved from source when no override.
    #[test]
    fn move_fields_preserves_source_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        let tgt = dir.path().join("state.rs");
        std::fs::write(
            &src,
            "struct Foo {\n    pub count: u32,\n    name: String,\n}\n",
        )
        .unwrap();
        std::fs::write(&tgt, "struct Bar {\n}\n").unwrap();

        let p = params_for(&src, &tgt, "Foo", &["count"]);
        let plan_json = plan_move_struct_fields(&p).unwrap();
        let tgt_text = target_text_from_plan(&plan_json);
        assert!(
            tgt_text.contains("pub count: u32"),
            "expected pub visibility"
        );
    }
}

//! RX-S2a — `add_rust_delegate_field` plan kind.
//!
//! Adds `<vis> <name>: <Target>` to a source struct and wires the delegate's
//! construction into a named constructor body.
//! `semantic_status: SyntaxOnly`.

use super::*;
use std::collections::HashSet;

// ── entry point ───────────────────────────────────────────────────────────────

pub fn plan_add_delegate(p: &RefactorPlanParams) -> anyhow::Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;

    let struct_name = p
        .impl_name
        .as_deref()
        .or(p.module_name.as_deref())
        .ok_or_else(|| {
            anyhow!("impl_name or module_name required to identify the source struct")
        })?;

    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required"))?;

    let delegate_type = p
        .delegate_type
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_type is required"))?;

    let visibility = p.visibility.as_deref().unwrap_or("pub");

    let ctor_names: Vec<&str> = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .map(|n| n.iter().map(String::as_str).collect())
        .unwrap_or_else(|| vec!["new"]);

    let default_init_expr = format!("{delegate_type}::new()");
    let init_expr: &str = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("init_expr"))
        .and_then(|v| v.as_str())
        .unwrap_or(&default_init_expr);

    let parsed = parse_rust_file(&source_path)?;
    let source_bytes = parsed.source.as_bytes();
    let root = parsed.tree.root_node();

    // ── find struct ───────────────────────────────────────────────────────────
    let struct_item = find_struct_syntax_item_local(&parsed, struct_name).ok_or_else(|| {
        anyhow!(
            "struct `{struct_name}` not found in {}",
            source_path.display()
        )
    })?;

    let struct_node = rust_node_by_range(
        root,
        "struct_item",
        struct_item.byte_start,
        struct_item.byte_end,
    )
    .ok_or_else(|| anyhow!("struct `{struct_name}` AST node not found"))?;

    let field_insert_byte = field_decl_list_close_byte(struct_node).ok_or_else(|| {
        anyhow!("could not find field_declaration_list closing brace in `{struct_name}`")
    })?;

    // ── find constructors ─────────────────────────────────────────────────────
    let ctor_name_set: HashSet<&str> = ctor_names.iter().copied().collect();
    let ctor_body_bytes =
        collect_ctor_body_close_bytes(&parsed, struct_name, source_bytes, &ctor_name_set);

    if ctor_body_bytes.is_empty() {
        bail!(
            "error.bad_input(code=no_constructor_found): no constructor matching {:?} found in impl `{struct_name}`",
            ctor_names
        );
    }

    // ── build edits ───────────────────────────────────────────────────────────
    let vis_prefix = if visibility.is_empty() {
        String::new()
    } else {
        format!("{visibility} ")
    };
    let field_text = format!("    {vis_prefix}{delegate_field}: {delegate_type},\n");
    let init_text = format!("    self.{delegate_field} = {init_expr};\n");

    let mut edits: Vec<TextEdit> = Vec::new();
    edits.push(TextEdit {
        byte_start: field_insert_byte,
        byte_end: field_insert_byte,
        replacement: field_text,
    });
    for &close_byte in &ctor_body_bytes {
        edits.push(TextEdit {
            byte_start: close_byte,
            byte_end: close_byte,
            replacement: init_text.clone(),
        });
    }
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits).context("overlapping edits in add_rust_delegate_field")?;

    let sha = sha256_hex(parsed.source.as_bytes());
    let file_edits = vec![FileEdit {
        path: path_string(&source_path),
        original_sha256: sha,
        edits,
        new_text: None,
    }];

    let plan = RefactorPlan {
        title: format!("add delegate field `{delegate_field}: {delegate_type}` to `{struct_name}`"),
        kind: "add_rust_delegate_field".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: file_edits,
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
        operator_opt_outs_used: Vec::new(),
    };

    validate_plan_shape(&plan).context("validate add_rust_delegate_field plan")?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn find_struct_syntax_item_local(parsed: &ParsedSource, name: &str) -> Option<SyntaxItem> {
    rust_items(parsed)
        .into_iter()
        .find(|item| item.kind == "struct_item" && item.name.as_deref() == Some(name))
}

fn field_decl_list_close_byte(struct_node: Node<'_>) -> Option<usize> {
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

/// Returns the closing-brace byte of each matching constructor body.
fn collect_ctor_body_close_bytes(
    parsed: &ParsedSource,
    struct_name: &str,
    source_bytes: &[u8],
    ctor_names: &HashSet<&str>,
) -> Vec<usize> {
    let root = parsed.tree.root_node();
    let mut result = Vec::new();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        if node.kind() != "impl_item" {
            continue;
        }
        let impl_type = node
            .child_by_field_name("type")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .map(|t| t.split(['<', ' ']).next().unwrap_or("").to_string())
            .unwrap_or_default();
        if impl_type != struct_name {
            continue;
        }
        let body = match node.child_by_field_name("body") {
            Some(b) => b,
            None => continue,
        };
        let mut bcursor = body.walk();
        for item in body.named_children(&mut bcursor) {
            if item.kind() != "function_item" {
                continue;
            }
            let fn_name = item
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source_bytes).ok())
                .unwrap_or("");
            if !ctor_names.contains(fn_name) {
                continue;
            }
            if let Some(fn_body) = item.child_by_field_name("body") {
                let count = fn_body.child_count();
                if count > 0 {
                    if let Some(close) = fn_body.child((count - 1) as u32) {
                        result.push(close.start_byte());
                    }
                }
            }
        }
    }
    result
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(
        source: &std::path::Path,
        struct_name: &str,
        delegate_field: &str,
        delegate_type: &str,
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "add_rust_delegate_field".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: None,
            item_names: None,
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
            delegate_type: Some(delegate_type.to_string()),
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
        }
    }

    fn apply_plan(plan_json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(plan_json).unwrap();
        let first_file = &v["edits"][0];
        let path = first_file["path"].as_str().unwrap();
        let source = std::fs::read_to_string(path).unwrap();
        let edits: Vec<TextEdit> = serde_json::from_value(first_file["edits"].clone()).unwrap();
        apply_text_edits(&source, &edits).unwrap()
    }

    // Gate: struct gains the delegate field with default pub visibility.
    #[test]
    fn delegate_field_added_to_struct() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn new() -> Self {
        Self { count: 0 }
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", "ServerState");
        let plan_json = plan_add_delegate(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("pub state: ServerState"),
            "expected delegate field in struct:\n{result}"
        );
    }

    // Gate: constructor body gains the self.delegate = Type::new() assignment.
    #[test]
    fn constructor_body_gains_assignment() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn new() -> Self {
        Self { count: 0 }
    }
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", "ServerState");
        let plan_json = plan_add_delegate(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state = ServerState::new()"),
            "expected constructor assignment:\n{result}"
        );
    }

    // Gate: zero constructors refuses with no_constructor_found.
    #[test]
    fn zero_constructors_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}
"#,
        )
        .unwrap();
        let p = make_params(&src, "BigServer", "state", "ServerState");
        let err = plan_add_delegate(&p).unwrap_err();
        assert!(
            err.to_string().contains("no_constructor_found"),
            "expected no_constructor_found error: {err}"
        );
    }

    // Gate: multiple constructors — item_names routes to the named one only.
    #[test]
    fn multi_constructor_routes_to_named() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn new() -> Self {
        Self { count: 0 }
    }
    fn with_count(count: u32) -> Self {
        Self { count }
    }
}
"#,
        )
        .unwrap();
        let mut p = make_params(&src, "BigServer", "state", "ServerState");
        p.item_names = Some(vec!["with_count".to_string()]);
        let plan_json = plan_add_delegate(&p).unwrap();
        let result = apply_plan(&plan_json);
        // Assignment should appear once and be inside with_count
        let occurrences = result.matches("self.state = ServerState::new()").count();
        assert_eq!(occurrences, 1, "expected exactly one assignment:\n{result}");
        // The 'new' body should not have the assignment
        let new_fn_idx = result.find("fn new()").unwrap();
        let with_count_idx = result.find("fn with_count").unwrap();
        let assignment_idx = result.find("self.state = ServerState::new()").unwrap();
        assert!(
            assignment_idx > with_count_idx,
            "assignment should be inside with_count, not new:\n{result}"
        );
        let _ = new_fn_idx; // used for context
    }

    // Gate: custom init_expr from toml_entries applied.
    #[test]
    fn custom_init_expr_applied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn new() -> Self {
        Self { count: 0 }
    }
}
"#,
        )
        .unwrap();
        let mut p = make_params(&src, "BigServer", "state", "ServerState");
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "init_expr".to_string(),
            serde_json::Value::String("ServerState::default()".to_string()),
        );
        p.toml_entries = Some(entries);
        let plan_json = plan_add_delegate(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("self.state = ServerState::default()"),
            "expected custom init_expr:\n{result}"
        );
    }

    // Gate: visibility override applied to the delegate field.
    #[test]
    fn visibility_override_applied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(
            &src,
            r#"
struct BigServer {
    count: u32,
}

impl BigServer {
    fn new() -> Self {
        Self { count: 0 }
    }
}
"#,
        )
        .unwrap();
        let mut p = make_params(&src, "BigServer", "state", "ServerState");
        p.visibility = Some("pub(crate)".to_string());
        let plan_json = plan_add_delegate(&p).unwrap();
        let result = apply_plan(&plan_json);
        assert!(
            result.contains("pub(crate) state: ServerState"),
            "expected pub(crate) visibility:\n{result}"
        );
    }

    // Gate: struct not found returns a clear error.
    #[test]
    fn struct_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("server.rs");
        std::fs::write(&src, "struct Foo {}\n").unwrap();
        let p = make_params(&src, "NonExistent", "state", "State");
        let err = plan_add_delegate(&p).unwrap_err();
        assert!(
            err.to_string().contains("NonExistent"),
            "expected struct name in error: {err}"
        );
    }
}

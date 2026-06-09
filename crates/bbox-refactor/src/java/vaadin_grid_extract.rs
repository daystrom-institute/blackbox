//! `java_vaadin_extract_grid_component` — conservative v1 Vaadin Grid
//! extraction.
//!
//! Pulls a `Grid<T>` field plus optional surrounding methods and data-provider
//! fields into a new component class. The target gains a small public API
//! shaped by what the planner can prove from the source: always `refresh()`,
//! plus `setItems(Collection<T>)` when the grid's generic argument can be read
//! textually from the field declaration. Refuses when the grid field is read
//! or written outside the methods the operator listed for the move, unless an
//! explicit public API is provided.

use super::*;

pub(crate) fn plan_java_vaadin_extract_grid_component(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!(
                "error.bad_input(code=target_required): target is required for \
                 java_vaadin_extract_grid_component"
            )
        })
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("error.bad_input(code=same_path): source and target must be different files");
    }
    if p.module_name.as_deref().is_none() {
        bail!(
            "error.bad_input(code=module_name_required): module_name (target component class name) \
             is required for java_vaadin_extract_grid_component"
        );
    }

    let candidate_fields = p
        .candidate_id
        .as_deref()
        .and_then(candidate_grid_fields_from_id)
        .unwrap_or_default();
    let grid_field = p
        .grid_field
        .as_deref()
        .map(str::to_string)
        .or_else(|| candidate_fields.first().cloned());
    let factory_method = p.factory_method.as_deref().map(str::to_string);
    if grid_field.is_none() && factory_method.is_none() {
        bail!(
            "error.bad_input(code=grid_anchor_required): pass `grid_field` naming the source \
             `Grid<T>` field, or `factory_method` naming the method that builds the grid"
        );
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_extract_grid_component only supports java files");
    }

    let target_class_name = java_target_type_name(p, &target_path)?;
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?;
    let source_class_name = java_class_name(class_node, &parsed.source);

    let method_names = p.item_names.as_deref().unwrap_or_default();
    let selected_methods = if method_names.is_empty() {
        Vec::new()
    } else {
        select_java_methods_by_name(&parsed, method_names)?
    };

    // Verify the named grid field exists and looks like Grid<...>.
    let mut grid_field_type: Option<String> = None;
    let mut grid_element_type: Option<String> = None;
    if let Some(name) = grid_field.as_deref() {
        let candidates = select_java_fields_by_name(&parsed, &[name.to_string()])?;
        let field = candidates
            .first()
            .ok_or_else(|| anyhow!("grid field `{name}` not found"))?;
        let ty = field.type_name.trim().to_string();
        if !looks_like_grid_type(&ty) {
            bail!(
                "error.bad_input(code=grid_field_type_mismatch): field `{name}` has type `{ty}`, \
                 which is not a Vaadin `Grid<...>` shape. Refusing."
            );
        }
        grid_element_type = grid_generic_argument(&ty);
        grid_field_type = Some(ty);
    }
    if let Some(row_type) = p.row_type.as_deref().filter(|s| !s.trim().is_empty()) {
        grid_element_type = Some(row_type.trim().to_string());
    }

    // Verify the named factory method exists when given.
    if let Some(factory) = factory_method.as_deref() {
        let _ = select_java_methods_by_name(&parsed, &[factory.to_string()])?;
    }

    // Compose the set of source field names to move: grid field + any extras
    // from `data_provider_fields` (or legacy `move_fields` as a fallback).
    let extra_field_names = p
        .data_provider_fields
        .as_deref()
        .or(p.move_fields.as_deref())
        .map(|fields| fields.to_vec())
        .unwrap_or_else(|| candidate_fields.iter().skip(1).cloned().collect());
    let mut all_field_names: Vec<String> = Vec::new();
    if let Some(name) = grid_field.as_deref() {
        all_field_names.push(name.to_string());
    }
    for f in &extra_field_names {
        if !all_field_names.contains(f) {
            all_field_names.push(f.clone());
        }
    }
    let selected_fields = if all_field_names.is_empty() {
        Vec::new()
    } else {
        select_java_fields_by_name(&parsed, &all_field_names)?
    };

    // Reference scope check on the grid field: every read/write in the source
    // class must be inside a moved method (item_names) or the factory method.
    // Otherwise refuse, unless operator supplies a public API list on the new
    // component and will rewire callers manually.
    let acknowledged = p
        .public_methods
        .as_deref()
        .map(|methods| !methods.is_empty())
        .unwrap_or(false);
    if let Some(field_name) = grid_field.as_deref() {
        let mut allowed_methods: Vec<&str> = selected_methods
            .iter()
            .filter_map(|m| m.item.name.as_deref())
            .collect();
        if let Some(name) = factory_method.as_deref() {
            allowed_methods.push(name);
        }
        let leaked = grid_field_referenced_outside(&parsed, field_name, &allowed_methods);
        if !leaked.is_empty() && !acknowledged {
            bail!(
                "error.bad_input(code=grid_field_referenced_outside_selection): grid field \
                 `{field_name}` is also read/written in source method(s) {leaked:?} that are not \
                 part of the move. Either add those method names to `item_names`, or pass \
                 `public_methods` to acknowledge that you will expose a public API on \
                 `{target_class_name}` and rewire the leaked callers yourself."
            );
        }
    }

    // Source edits: delete moved methods and fields.
    let mut delete_records: Vec<(usize, usize, String)> = Vec::new();
    for field in &selected_fields {
        let s = field.item.leading_trivia_start;
        let e = field.item.byte_end;
        delete_records.push((s, e, parsed.source[s..e].to_string()));
    }
    for method in &selected_methods {
        let s = method.item.leading_trivia_start;
        let e = method.item.byte_end;
        delete_records.push((s, e, parsed.source[s..e].to_string()));
    }
    if let Some(factory) = factory_method.as_deref() {
        // Move the factory method when not already in item_names.
        if !method_names.iter().any(|n| n == factory) {
            let extra = select_java_methods_by_name(&parsed, &[factory.to_string()])?;
            for method in &extra {
                let s = method.item.leading_trivia_start;
                let e = method.item.byte_end;
                delete_records.push((s, e, parsed.source[s..e].to_string()));
            }
        }
    }

    let mut source_edits: Vec<TextEdit> = delete_records
        .iter()
        .map(|(s, e, _)| TextEdit {
            byte_start: *s,
            byte_end: *e,
            replacement: String::new(),
        })
        .collect();

    let delegate_insert_pos = java_class_body_insert_position(class_node, &parsed.source);
    let delegate_field_name = default_grid_delegate_field(&target_class_name);
    let delegate_decl = format!(
        "\n    private final {target_class_name} {delegate_field_name} = new {target_class_name}();\n"
    );
    source_edits.push(TextEdit {
        byte_start: delegate_insert_pos,
        byte_end: delegate_insert_pos,
        replacement: delegate_decl,
    });
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Target file: pick a base (Composite<Div> by default), inject body, add API.
    let component_base = p
        .component_base
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Composite<Div>");

    let mut target_body_chunks: Vec<(usize, String)> = delete_records
        .iter()
        .map(|(s, _, t)| (*s, t.clone()))
        .collect();
    target_body_chunks.sort_by_key(|(s, _)| *s);

    let resolved_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let mut target_prelude =
        java_default_target_prelude(p, &parsed.source, resolved_pkg.as_deref());
    for fqcn in vaadin_imports_for_base(component_base) {
        if !target_prelude.contains(&format!("import {fqcn};")) {
            target_prelude = inject_import_into_prelude(target_prelude, fqcn);
        }
    }

    // Build the public API additions.
    let mut api_methods = String::new();
    if let Some(name) = grid_field.as_deref() {
        api_methods.push_str(&format!(
            "\n    public void refresh() {{\n        {name}.getDataProvider().refreshAll();\n    }}\n"
        ));
        if let Some(elem) = grid_element_type.as_deref() {
            api_methods.push_str(&format!(
                "\n    public void setItems(java.util.Collection<{elem}> items) {{\n        {name}.setItems(items);\n    }}\n"
            ));
        }
    }

    let body_text = target_body_chunks
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join("\n\n");
    let combined_body = if api_methods.is_empty() {
        body_text
    } else if body_text.is_empty() {
        api_methods
    } else {
        format!("{body_text}\n{api_methods}")
    };

    let mut target_text = java_class_wrapper(&target_class_name, &target_prelude, &combined_body);
    target_text = inject_extends(&target_text, &target_class_name, component_base);

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_text,
        }],
        new_text: None,
    };

    let api_summary = match (grid_field.as_deref(), grid_element_type.as_deref()) {
        (Some(_), Some(elem)) => format!("refresh() + setItems(Collection<{elem}>)"),
        (Some(_), None) => "refresh()".to_string(),
        _ => "(no auto-API; factory-only)".to_string(),
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract Vaadin Grid from `{source_class_name}` to `{target_class_name}` \
             (extends {component_base}); API: {api_summary}",
        ),
        kind: "java_vaadin_extract_grid_component".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            target_edit,
        ],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&source_path),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&target_path),
                byte_range: None,
            },
        ],
        items: Vec::new(),
        leftovers: vec![
            format!(
                "v1 grid extract; component base default `{component_base}`. Grid field type was \
                 detected as `{ty}`.",
                ty = grid_field_type.unwrap_or_else(|| "(no grid field; factory-only)".to_string()),
            ),
            "Public API generation is conservative: `refresh()` always, `setItems(Collection<T>)` \
             only when the grid generic argument is unambiguously readable from the field \
             declaration."
                .to_string(),
            "Callers of the original grid field on the source class are NOT rewritten; expose \
             them through the new component's public API after apply."
                .to_string(),
        ],
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

fn looks_like_grid_type(ty: &str) -> bool {
    let t = ty.trim();
    t == "Grid" || t.starts_with("Grid<") || t.starts_with("com.vaadin.flow.component.grid.Grid")
}

fn candidate_grid_fields_from_id(candidate_id: &str) -> Option<Vec<String>> {
    let (kind, members) = candidate_id.split_once(':')?;
    if kind != "grid-factory" {
        return None;
    }
    let fields: Vec<String> = members
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

fn grid_generic_argument(ty: &str) -> Option<String> {
    let trimmed = ty.trim();
    let open = trimmed.find('<')?;
    let close = trimmed.rfind('>')?;
    if close <= open + 1 {
        return None;
    }
    let inner = trimmed[open + 1..close].trim();
    // Reject obviously compound inner expressions; we only emit a setItems
    // helper when the element type is a single identifier (possibly dotted).
    if inner.is_empty() || inner.contains(',') || inner.contains('<') {
        return None;
    }
    if !inner
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '$')
    {
        return None;
    }
    Some(inner.to_string())
}

fn vaadin_imports_for_base(base: &str) -> Vec<&'static str> {
    let t = base.trim();
    if t.starts_with("Composite<Div>") {
        return vec![
            "com.vaadin.flow.component.Composite",
            "com.vaadin.flow.component.html.Div",
        ];
    }
    if t.starts_with("Composite<") {
        return vec!["com.vaadin.flow.component.Composite"];
    }
    match t {
        "VerticalLayout" => vec!["com.vaadin.flow.component.orderedlayout.VerticalLayout"],
        "HorizontalLayout" => vec!["com.vaadin.flow.component.orderedlayout.HorizontalLayout"],
        "Div" => vec!["com.vaadin.flow.component.html.Div"],
        _ => Vec::new(),
    }
}

fn default_grid_delegate_field(target_class_name: &str) -> String {
    let mut chars = target_class_name.chars();
    match chars.next() {
        Some(first) => {
            let mut s: String = first.to_lowercase().collect();
            s.push_str(chars.as_str());
            s
        }
        None => "gridComponent".to_string(),
    }
}

/// Best-effort textual scan: collect names of methods in the source class
/// whose body mentions `field_name` outside the allowed-methods list.
/// Conservative — looks at method bodies only, with simple identifier
/// boundary checks.
fn grid_field_referenced_outside(
    parsed: &ParsedSource,
    field_name: &str,
    allowed: &[&str],
) -> Vec<String> {
    let source = parsed.source.as_str();
    let methods = java_methods(parsed);
    let mut leaked = Vec::new();
    for m in &methods {
        let Some(name) = m.item.name.as_deref() else {
            continue;
        };
        if allowed.contains(&name) {
            continue;
        }
        let body = &source[m.item.byte_start..m.item.byte_end];
        if identifier_appears(body, field_name) {
            leaked.push(name.to_string());
        }
    }
    leaked
}

fn identifier_appears(text: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let ident_bytes = ident.as_bytes();
    let mut i = 0;
    while i + ident_bytes.len() <= bytes.len() {
        if &bytes[i..i + ident_bytes.len()] == ident_bytes {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = i + ident_bytes.len();
            let after_ok = after == bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

fn inject_import_into_prelude(prelude: String, fqcn: &str) -> String {
    let line = format!("import {fqcn};");
    if prelude.contains(&line) {
        return prelude;
    }
    let mut out = String::new();
    let mut inserted = false;
    for piece in prelude.split_inclusive('\n') {
        out.push_str(piece);
        if !inserted && piece.trim_start().starts_with("package ") {
            out.push_str(&line);
            out.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        let mut prefix = String::new();
        prefix.push_str(&line);
        prefix.push_str("\n\n");
        prefix.push_str(&out);
        return prefix;
    }
    out
}

fn inject_extends(target_text: &str, class_name: &str, base: &str) -> String {
    let needle = format!("public class {class_name}");
    let Some(pos) = target_text.find(&needle) else {
        return target_text.to_string();
    };
    let after = pos + needle.len();
    let Some(brace_rel) = target_text[after..].find('{') else {
        return target_text.to_string();
    };
    let brace_at = after + brace_rel;
    let between = target_text[after..brace_at].trim();
    if !between.is_empty() {
        return target_text.to_string();
    }
    let mut out = String::with_capacity(target_text.len() + base.len() + 16);
    out.push_str(&target_text[..after]);
    out.push_str(&format!(" extends {base} "));
    out.push_str(&target_text[brace_at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base_params(source: &Path, target: &Path) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "java_vaadin_extract_grid_component".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("UserGridComponent".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_when_no_grid_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(&src, "package p;\npublic class Foo {}\n").unwrap();
        let params = base_params(&src, &tgt);
        let err = plan_java_vaadin_extract_grid_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("grid_anchor_required"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_when_named_field_not_grid_type() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private String users;\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.grid_field = Some("users".to_string());
        let err = plan_java_vaadin_extract_grid_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("grid_field_type_mismatch"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_grid_leak_without_acknowledgment() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private Grid<User> grid;\n    \
             void setup() { grid.setItems(java.util.List.of()); }\n    \
             void external() { System.out.println(grid); }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.grid_field = Some("grid".to_string());
        params.item_names = Some(vec!["setup".to_string()]);
        let err = plan_java_vaadin_extract_grid_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("grid_field_referenced_outside_selection"),
            "got: {err}"
        );
    }

    #[test]
    fn happy_path_emits_refresh_and_setitems_api() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private Grid<User> grid;\n    \
             void setup() { grid.setItems(java.util.List.of()); }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.grid_field = Some("grid".to_string());
        params.item_names = Some(vec!["setup".to_string()]);
        let json = plan_java_vaadin_extract_grid_component(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let target_replacement = v["edits"][1]["edits"][0]["replacement"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            target_replacement.contains("public void refresh()"),
            "refresh missing: {target_replacement}"
        );
        assert!(
            target_replacement.contains("public void setItems(java.util.Collection<User> items)"),
            "setItems<User> missing: {target_replacement}"
        );
        assert!(
            target_replacement.contains("public class UserGridComponent extends Composite<Div>"),
            "extends clause wrong: {target_replacement}"
        );
    }

    #[test]
    fn leak_with_public_methods_acknowledgment_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private Grid<User> grid;\n    \
             void setup() { grid.setItems(java.util.List.of()); }\n    \
             void external() { System.out.println(grid); }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.grid_field = Some("grid".to_string());
        params.item_names = Some(vec!["setup".to_string()]);
        params.public_methods = Some(vec!["refresh".to_string()]);
        let json = plan_java_vaadin_extract_grid_component(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["plan_status"], "planned");
    }

    #[test]
    fn factory_only_extract_skips_setitems() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("UserGridComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    \
             Grid<User> buildGrid() { return new Grid<>(); }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.factory_method = Some("buildGrid".to_string());
        let json = plan_java_vaadin_extract_grid_component(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let target_replacement = v["edits"][1]["edits"][0]["replacement"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !target_replacement.contains("setItems(java.util.Collection"),
            "factory-only path should not auto-emit setItems: {target_replacement}"
        );
        assert!(
            target_replacement.contains("buildGrid"),
            "factory method body not moved: {target_replacement}"
        );
    }
}

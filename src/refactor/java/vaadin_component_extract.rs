//! `java_vaadin_extract_component` — conservative v1 component extraction.
//!
//! Carves a set of selected methods + fields out of a source class into a new
//! Vaadin component class (default base `VerticalLayout`, overridable via
//! `component_base`). Source-side, deletes the moved declarations and inserts a
//! delegate component field. Refuses route/lifecycle methods and silently
//! dropping scope annotations; those refusals are real, not stubs.

use super::*;

pub(crate) fn plan_java_vaadin_extract_component(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!(
                "error.bad_input(code=target_required): target is required for \
                 java_vaadin_extract_component"
            )
        })
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("error.bad_input(code=same_path): source and target must be different files");
    }
    if p.module_name.as_deref().is_none() {
        bail!(
            "error.bad_input(code=module_name_required): module_name (target component class name) \
             is required for java_vaadin_extract_component"
        );
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_extract_component only supports java files");
    }

    let method_names = p.item_names.clone().unwrap_or_default();
    let candidate_fields = p
        .candidate_id
        .as_deref()
        .and_then(candidate_fields_from_id)
        .unwrap_or_default();
    let field_names = p.move_fields.clone().unwrap_or(candidate_fields);
    if method_names.is_empty() && field_names.is_empty() {
        bail!(
            "error.bad_input(code=nothing_selected): pass at least one of `item_names` (methods) \
             or `move_fields` (fields), or a candidate_id from \
             java_vaadin_view_structure_analysis, to identify what to extract"
        );
    }

    let target_class_name = java_target_type_name(p, &target_path)?;
    let component_base = component_base_for(p).to_string();
    validate_component_base(&component_base)?;

    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?;
    let source_class_name = java_class_name(class_node, &parsed.source);

    // Refuse silently dropping a class-level scope/route annotation unless the
    // operator passed an explicit override (target_scope carrying their
    // chosen annotation for the new component).
    let source_scope_annos = detect_class_scope_annotations(class_node, &parsed.source);
    let has_target_scope_override = p
        .target_scope
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !source_scope_annos.is_empty() && !has_target_scope_override {
        bail!(
            "error.bad_input(code=source_scope_annotation_without_target_scope): source class \
             `{source_class_name}` carries scope/route annotation(s) {source_scope_annos:?}; \
             extracting into a new component would silently drop them. Pass `target_scope` \
             naming the scope annotation/policy you want on the new component to acknowledge."
        );
    }

    let selected_methods = if method_names.is_empty() {
        Vec::new()
    } else {
        select_java_methods_by_name(&parsed, &method_names)?
    };
    for method in &selected_methods {
        let declaration_text = &parsed.source[method.item.byte_start..method.item.byte_end];
        if declaration_has_routing_annotation(declaration_text) {
            bail!(
                "error.bad_input(code=route_method_in_extract): method `{}` carries a routing or \
                 page-title annotation; extracting it into a component would break routing. \
                 Refusing.",
                method.item.name.as_deref().unwrap_or("(unnamed)")
            );
        }
        if let Some(name) = method.item.name.as_deref() {
            if is_lifecycle_method(name) {
                bail!(
                    "error.bad_input(code=lifecycle_method_in_extract): method `{name}` looks \
                     like a Vaadin/Flow lifecycle hook (onAttach/onDetach/beforeEnter/...); \
                     extracting it into a separate component would silently disable it. \
                     Refusing."
                );
            }
        }
    }

    let selected_fields = if field_names.is_empty() {
        Vec::new()
    } else {
        select_java_fields_by_name(&parsed, &field_names)?
    };

    let mut delete_records: Vec<(usize, usize, String)> = Vec::new();
    for field in &selected_fields {
        let start = field.item.leading_trivia_start;
        let end = field.item.byte_end;
        let text = parsed.source[start..end].to_string();
        delete_records.push((start, end, text));
    }
    for method in &selected_methods {
        let start = method.item.leading_trivia_start;
        let end = method.item.byte_end;
        let text = parsed.source[start..end].to_string();
        delete_records.push((start, end, text));
    }

    let mut sorted_for_target: Vec<(usize, String)> = delete_records
        .iter()
        .map(|(start, _, text)| (*start, text.clone()))
        .collect();
    sorted_for_target.sort_by_key(|(start, _)| *start);
    let target_body_chunks: Vec<String> = sorted_for_target.into_iter().map(|(_, t)| t).collect();

    let mut source_edits: Vec<TextEdit> = delete_records
        .iter()
        .map(|(s, e, _)| TextEdit {
            byte_start: *s,
            byte_end: *e,
            replacement: String::new(),
        })
        .collect();

    // Compute the source-side insert position BEFORE applying deletes. Place
    // the delegate component field right after the opening `{` so the insert
    // never sits inside any deleted range — even if we move every field.
    let delegate_insert_pos = java_class_body_insert_position(class_node, &parsed.source);
    let delegate_field_name = p
        .delegate_field
        .clone()
        .unwrap_or_else(|| default_delegate_field(&target_class_name));
    validate_java_member_name(&delegate_field_name, "delegate_field")?;

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

    // Target file content.
    let resolved_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let mut target_prelude =
        java_default_target_prelude(p, &parsed.source, resolved_pkg.as_deref());
    if let Some(base_import) = vaadin_import_for_base(&component_base) {
        if !target_prelude.contains(&format!("import {base_import};")) {
            target_prelude = inject_import_into_prelude(target_prelude, base_import);
        }
    }
    let body = target_body_chunks.join("\n\n");
    let mut target_text = java_class_wrapper(&target_class_name, &target_prelude, &body);
    target_text = inject_extends(&target_text, &target_class_name, &component_base);

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

    let mut leftovers = vec![
        format!(
            "v1 conservative component extract; default component base `{component_base}` \
             unless overridden via `component_base`."
        ),
        format!(
            "Source-side delegate field `{delegate_field_name}` is wired with `new \
             {target_class_name}()`; if the extracted component needs constructor \
             arguments, edit the source after apply."
        ),
        "Cross-class instance method call rewriting and parent.add(delegate) wiring are \
         out of v1 scope."
            .to_string(),
    ];
    if let Some(parameters_type_name) = p
        .parameters_type_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        leftovers.push(format!(
            "Requested parameter/context type `{}` is recorded for operator follow-up; \
             v1 does not synthesize parameter records.",
            parameters_type_name.trim()
        ));
    }
    if let Some(target_access_policy) = p
        .target_access_policy
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        leftovers.push(format!(
            "Target access policy `{}` is operator-supplied but component extraction does \
             not generate route/access annotations.",
            target_access_policy.trim()
        ));
    }

    let plan = RefactorPlan {
        title: format!(
            "Extract {} method(s) and {} field(s) from `{source_class_name}` to component \
             `{target_class_name}` (extends {component_base})",
            selected_methods.len(),
            selected_fields.len(),
        ),
        kind: "java_vaadin_extract_component".to_string(),
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
        leftovers,
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

fn component_base_for(p: &RefactorPlanParams) -> &str {
    p.component_base
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("VerticalLayout")
}

fn candidate_fields_from_id(candidate_id: &str) -> Option<Vec<String>> {
    let (kind, members) = candidate_id.split_once(':')?;
    if !matches!(
        kind,
        "component-section" | "grid-factory" | "dialog-controller"
    ) {
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

fn validate_component_base(base: &str) -> Result<()> {
    // Conservative shape check — accept simple Java identifiers and the
    // common `Composite<...>` generic spelling. Reject anything else so we
    // don't paste arbitrary text into the class header.
    let trimmed = base.trim();
    if trimmed.is_empty() {
        bail!("component_base must not be empty");
    }
    let acceptable = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '<' | '>' | ',' | ' ' | '.'));
    if !acceptable {
        bail!(
            "error.bad_input(code=invalid_component_base): component_base `{base}` is not a \
             plausible Java type expression"
        );
    }
    Ok(())
}

fn vaadin_import_for_base(base: &str) -> Option<&'static str> {
    match base.trim() {
        "VerticalLayout" => Some("com.vaadin.flow.component.orderedlayout.VerticalLayout"),
        "HorizontalLayout" => Some("com.vaadin.flow.component.orderedlayout.HorizontalLayout"),
        "Div" => Some("com.vaadin.flow.component.html.Div"),
        b if b.starts_with("Composite<") => Some("com.vaadin.flow.component.Composite"),
        _ => None,
    }
}

fn detect_class_scope_annotations(class_node: Node<'_>, source: &str) -> Vec<String> {
    let mut annotations = Vec::new();
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let mut modifier_cursor = child.walk();
        for modifier in child.named_children(&mut modifier_cursor) {
            if matches!(modifier.kind(), "annotation" | "marker_annotation") {
                if let Ok(text) = modifier.utf8_text(source.as_bytes()) {
                    annotations.push(text.trim().to_string());
                }
            }
        }
    }

    annotations
        .into_iter()
        .filter(|l| {
            l.contains("UIScope")
                || l.contains("VaadinSessionScope")
                || l.contains("RouteScope")
                || l.contains("@Route")
                || l.contains("@RouteAlias")
                || l.contains("@PageTitle")
                || l.contains("@SpringComponent")
                || l.contains("@SessionScope")
        })
        .collect()
}

fn declaration_has_routing_annotation(declaration_text: &str) -> bool {
    declaration_text.lines().map(str::trim).any(|l| {
        l.starts_with("@Route")
            || l.starts_with("@RouteAlias")
            || l.starts_with("@PageTitle")
            || l.starts_with("@PreserveOnRefresh")
    })
}

fn is_lifecycle_method(name: &str) -> bool {
    matches!(
        name,
        "onAttach"
            | "onDetach"
            | "beforeEnter"
            | "beforeLeave"
            | "afterNavigation"
            | "onEnter"
            | "onLocationChange"
    )
}

fn default_delegate_field(target_class_name: &str) -> String {
    let mut chars = target_class_name.chars();
    match chars.next() {
        Some(first) => {
            let mut s: String = first.to_lowercase().collect();
            s.push_str(chars.as_str());
            s
        }
        None => "delegate".to_string(),
    }
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
        // Already has extends/implements — leave alone.
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
            kind: "java_vaadin_extract_component".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            module_name: Some("ExtractedComponent".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_when_target_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        fs::write(&src, "package p;\npublic class Foo { void m() {} }\n").unwrap();
        let mut params = base_params(&src, &dir.path().join("X.java"));
        params.target = None;
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(format!("{err}").contains("target_required"), "got: {err}");
    }

    #[test]
    fn refuses_when_module_name_missing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ExtractedComponent.java");
        fs::write(&src, "package p;\npublic class Foo { void m() {} }\n").unwrap();
        let mut params = base_params(&src, &tgt);
        params.module_name = None;
        params.item_names = Some(vec!["m".to_string()]);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("module_name_required"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_when_nothing_selected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("Out.java");
        fs::write(&src, "package p;\npublic class Foo { void m() {} }\n").unwrap();
        let params = base_params(&src, &tgt);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(format!("{err}").contains("nothing_selected"), "got: {err}");
    }

    #[test]
    fn refuses_route_annotated_method() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("Out.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    @Route(\"x\")\n    void m() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["m".to_string()]);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("route_method_in_extract"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_lifecycle_method() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("Out.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    void onAttach() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["onAttach".to_string()]);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("lifecycle_method_in_extract"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_source_scope_annotation_without_override() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("Out.java");
        fs::write(
            &src,
            "package p;\n@Route(\"/foo\")\npublic class Foo {\n    void m() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["m".to_string()]);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(
            format!("{err}").contains("source_scope_annotation_without_target_scope"),
            "got: {err}"
        );
    }

    #[test]
    fn happy_path_plan_shape_for_method_only_extract() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ExtractedComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private int counter;\n    \
             public void greet() { System.out.println(\"hi\"); }\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["greet".to_string()]);
        let json = plan_java_vaadin_extract_component(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "java_vaadin_extract_component");
        assert_eq!(v["plan_status"], "planned");
        let edits = v["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        let target_replacement = edits[1]["edits"][0]["replacement"].as_str().unwrap();
        assert!(
            target_replacement.contains("public class ExtractedComponent extends VerticalLayout"),
            "target wrapper missing or extends absent: {target_replacement}"
        );
        assert!(
            target_replacement.contains("public void greet()"),
            "method body not copied: {target_replacement}"
        );
        assert!(
            target_replacement
                .contains("import com.vaadin.flow.component.orderedlayout.VerticalLayout;"),
            "base import missing: {target_replacement}"
        );
        // Source edits: one delete + one delegate insert.
        let source_edits = edits[0]["edits"].as_array().unwrap();
        assert!(source_edits.len() >= 2);
        let has_delegate_insert = source_edits.iter().any(|e| {
            e["replacement"].as_str().unwrap_or("").contains(
                "private final ExtractedComponent extractedComponent = new ExtractedComponent();",
            )
        });
        assert!(has_delegate_insert, "delegate field insert missing");
    }

    #[test]
    fn extract_field_only_plan_shape() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Foo.java");
        let tgt = dir.path().join("ExtractedComponent.java");
        fs::write(
            &src,
            "package p;\npublic class Foo {\n    private String label = \"hi\";\n    void m() {}\n}\n",
        )
        .unwrap();
        let mut params = base_params(&src, &tgt);
        params.move_fields = Some(vec!["label".to_string()]);
        let json = plan_java_vaadin_extract_component(&params).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let target_replacement = v["edits"][1]["edits"][0]["replacement"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            target_replacement.contains("private String label"),
            "field text not copied: {target_replacement}"
        );
    }

    #[test]
    fn non_java_source_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("foo.rs");
        let tgt = dir.path().join("out.rs");
        fs::write(&src, "fn main() {}\n").unwrap();
        let mut params = base_params(&src, &tgt);
        params.item_names = Some(vec!["main".to_string()]);
        let err = plan_java_vaadin_extract_component(&params).unwrap_err();
        assert!(format!("{err}").contains("java"), "got: {err}");
    }
}

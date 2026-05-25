//! `java_vaadin_provider_binding_generation` — ensure Guice provider
//! bindings for Vaadin request-scoped lookups.
//!
//! This is intentionally a small project-setup helper, not a caller rewrite.
//! It supports Guice module/config classes and refuses Spring or ambiguous
//! non-Guice integration styles so Vaadin static lookup rewrites can depend on
//! explicit `Provider<UI>` / `Provider<VaadinSession>` bindings.

use super::*;

pub(crate) fn plan_java_vaadin_provider_binding_generation(
    p: &RefactorPlanParams,
) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("java_vaadin_provider_binding_generation only supports java files");
    }
    if source_mentions_spring(&parsed.source) || project_mentions_spring(p, &source_path)? {
        bail!("java_vaadin_provider_binding_generation refuses Spring/Vaadin projects");
    }
    if !source_is_guice_module(&parsed.source) {
        bail!(
            "java_vaadin_provider_binding_generation requires a Guice module/config class; pass the module source file"
        );
    }
    if !source_mentions_vaadin(&parsed.source) && !project_mentions_vaadin(p, &source_path)? {
        bail!("java_vaadin_provider_binding_generation requires a Vaadin project");
    }

    let class_node = if let Some(class_name) = p.module_name.as_deref() {
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
    let class_name = java_class_simple_name(class_node, &parsed.source)
        .ok_or_else(|| anyhow!("target module class must be named"))?;
    let class_body = class_node
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("target class `{class_name}` has no class body"))?;

    let mut edits = Vec::new();
    push_missing_import(&mut edits, &parsed.source, "com.google.inject.Provides")?;
    push_missing_import(&mut edits, &parsed.source, "com.vaadin.flow.component.UI")?;
    push_missing_import(
        &mut edits,
        &parsed.source,
        "com.vaadin.flow.server.VaadinSession",
    )?;
    if provider_import_fqcn(&parsed.source).is_none() {
        push_missing_import(&mut edits, &parsed.source, "com.google.inject.Provider")?;
    }

    let mut methods = Vec::new();
    if !has_provider_binding_for(&parsed.source, "UI") {
        methods.push(("UI", "provideUiProvider", "UI::getCurrent", "Provider<UI>"));
    }
    if !has_provider_binding_for(&parsed.source, "VaadinSession") {
        methods.push((
            "VaadinSession",
            "provideVaadinSessionProvider",
            "VaadinSession::getCurrent",
            "Provider<VaadinSession>",
        ));
    }
    if !methods.is_empty() {
        let insert_at = class_body.end_byte().saturating_sub(1);
        let indent = class_member_indent(class_body, &parsed.source);
        let method_text = methods
            .into_iter()
            .map(|(_, name, supplier, return_type)| {
                format!(
                    "\n{indent}@Provides\n{indent}{return_type} {name}() {{\n{indent}    return {supplier};\n{indent}}}\n"
                )
            })
            .collect::<String>();
        edits.push(TextEdit {
            byte_start: insert_at,
            byte_end: insert_at,
            replacement: method_text,
        });
    }

    let mut edits = lombokify::merge_colocated_inserts(edits);
    edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&edits)?;

    let plan = RefactorPlan {
        title: if edits.is_empty() {
            format!("verify Vaadin provider bindings in `{class_name}`")
        } else {
            format!("ensure Vaadin Provider<UI>/Provider<VaadinSession> bindings in `{class_name}`")
        },
        kind: "java_vaadin_provider_binding_generation".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: if edits.is_empty() {
            Vec::new()
        } else {
            vec![FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits,
                new_text: None,
            }]
        },
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: vec![
            "Guice-only v1 helper; Spring/Vaadin projects are refused rather than rewritten."
                .to_string(),
            "Provider methods are added only when matching UI/VaadinSession Provider bindings are absent."
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

fn push_missing_import(edits: &mut Vec<TextEdit>, source: &str, fqcn: &str) -> Result<()> {
    let import_line = format!("import {fqcn};");
    if source.lines().any(|line| line.trim() == import_line) {
        return Ok(());
    }
    let simple = fqcn.rsplit('.').next().unwrap_or("");
    for line in source.lines() {
        if java_import_simple_name(line).as_deref() == Some(simple) {
            bail!("cannot add import `{fqcn}` because another `{simple}` import already exists");
        }
        if let Some(pkg) = java_import_wildcard_package(line) {
            if Some(pkg.as_str()) == java_fqcn_package(fqcn) {
                return Ok(());
            }
        }
    }
    if let Some(edit) = java_source_import_edit(source, fqcn) {
        edits.push(edit);
    }
    Ok(())
}

fn source_is_guice_module(source: &str) -> bool {
    source.contains("com.google.inject")
        || source.contains("extends AbstractModule")
        || source.contains("@Provides")
}

fn source_mentions_spring(source: &str) -> bool {
    source.contains("org.springframework") || source.contains("@SpringBootApplication")
}

fn source_mentions_vaadin(source: &str) -> bool {
    source.contains("com.vaadin.flow")
        || source.contains("VaadinSession")
        || source.contains("UI.getCurrent")
}

fn project_mentions_spring(p: &RefactorPlanParams, source_path: &Path) -> Result<bool> {
    project_contains_java_text(p, source_path, source_mentions_spring)
}

fn project_mentions_vaadin(p: &RefactorPlanParams, source_path: &Path) -> Result<bool> {
    project_contains_java_text(p, source_path, source_mentions_vaadin)
}

fn project_contains_java_text(
    p: &RefactorPlanParams,
    source_path: &Path,
    predicate: fn(&str) -> bool,
) -> Result<bool> {
    let root = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        });
    let mut stack = vec![root];
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "target" | "build" | ".gradle") {
                continue;
            }
            for entry in fs::read_dir(&path)
                .with_context(|| format!("reading directory {}", path.display()))?
            {
                stack.push(entry?.path());
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            let text = fs::read_to_string(&path)
                .with_context(|| format!("reading java source {}", path.display()))?;
            if predicate(&text) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn provider_import_fqcn(source: &str) -> Option<&'static str> {
    if source.contains("import jakarta.inject.Provider;") {
        Some("jakarta.inject.Provider")
    } else if source.contains("import javax.inject.Provider;") {
        Some("javax.inject.Provider")
    } else if source.contains("import com.google.inject.Provider;")
        || source.contains("import com.google.inject.*;")
    {
        Some("com.google.inject.Provider")
    } else {
        None
    }
}

fn has_provider_binding_for(source: &str, type_name: &str) -> bool {
    let compact = source.split_whitespace().collect::<String>();
    compact.contains(&format!("Provider<{type_name}>"))
        && (compact.contains(&format!("{type_name}::getCurrent"))
            || compact.contains(&format!("{type_name}.getCurrent")))
}

fn class_member_indent(class_body: Node<'_>, source: &str) -> String {
    let mut cursor = class_body.walk();
    for child in class_body.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "method_declaration" | "field_declaration" | "constructor_declaration"
        ) {
            let line_start = source[..child.start_byte()]
                .rfind('\n')
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let indent = &source[line_start..child.start_byte()];
            if indent.chars().all(|c| c == ' ' || c == '\t') && !indent.is_empty() {
                return indent.to_string();
            }
        }
    }
    let class_line_start = source[..class_body.start_byte()]
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let class_indent = &source[class_line_start..class_body.start_byte()];
    format!("{class_indent}    ")
}

fn java_class_simple_name(class_node: Node<'_>, source: &str) -> Option<String> {
    class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
}

use super::*;

pub(crate) fn plan_extract_java_methods(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_methods"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_methods only supports java files");
    }

    let names = p.item_names.as_deref().unwrap_or_default();
    let mut selected = select_java_methods_by_name(&parsed, names)?;
    let captured_variables = captured_fields_for_methods(&parsed, &selected);
    let dependency_report = if p.deep_analysis.unwrap_or(false) {
        analyze_extracted_dependencies(
            &parsed,
            &selected,
            p.project_dir.as_deref().map(Path::new),
        )
    } else {
        Default::default()
    };

    selected.sort_by_key(|m| std::cmp::Reverse(m.item.byte_start));

    let mut source_edits = Vec::new();
    let mut extracted_content = Vec::new();

    for method in &selected {
        source_edits.push(TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.byte_end,
            replacement: String::new(),
        });
        let content = &parsed.source[method.item.leading_trivia_start..method.item.byte_end];
        extracted_content.push(content.to_string());
    }

    extracted_content.reverse();

    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };

    let target_content = if target_path.exists() {
        let mut text = String::from_utf8(original_target_bytes.clone()).unwrap_or_default();
        // Gap 16: when the target already has content, the structural
        // append (insert method before final `}`) leaves the moved
        // method's type references unimported. Diff source's imports
        // against target's existing import block and inject each
        // missing one. java_inject_import is idempotent — duplicate
        // checks are baked in. Conservative: copies ALL imports from
        // source, not just the ones the moved method needs; unused
        // imports are a javac warning, not an error.
        let source_imports = extract_java_imports(&parsed.source);
        for import_line in &source_imports {
            if let Some(fqcn) = import_line
                .trim()
                .strip_prefix("import ")
                .and_then(|s| s.trim().strip_suffix(';'))
                .map(|s| s.trim().to_string())
            {
                if !fqcn.is_empty() {
                    text = java_inject_import(&text, &fqcn);
                }
            }
        }
        let insert_at = text.rfind('}').unwrap_or(text.len());
        text.insert_str(
            insert_at,
            &format!("\n{}\n", extracted_content.join("\n\n")),
        );
        text
    } else {
        let class_name = java_target_type_name(p, &target_path)?;
        let resolved_pkg =
            resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
        let prelude = java_default_target_prelude(p, &parsed.source, resolved_pkg.as_deref());
        java_class_wrapper(&class_name, &prelude, &extracted_content.join("\n\n"))
    };

    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
        new_text: None,
    };

    let plan = RefactorPlan {
        title: format!(
            "Extract {} methods to {}",
            selected.len(),
            target_path.display()
        ),
        kind: "extract_java_methods".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
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
        leftovers: Vec::new(),
        captured_variables,
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: dependency_report.external_calls,
        inherited_dependencies: dependency_report.inherited_dependencies,
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

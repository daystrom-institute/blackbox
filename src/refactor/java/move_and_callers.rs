use super::*;

pub(crate) fn plan_move_java_constant(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_java_constant"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let source_parsed = parse_source_file(&source_path)?;
    if source_parsed.language != "java" {
        bail!("move_java_constant only supports java files");
    }
    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names (constant names) is required for move_java_constant"))?;
    let visibility = p.visibility.as_deref().unwrap_or("private").to_string();
    validate_java_visibility(&visibility)?;
    let keep_copy = p.keep_copy.unwrap_or(false);

    // Match each name against a static-final field_declaration.
    let selected = select_java_static_final_fields_by_name(&source_parsed, names)?;

    // Build moved constant text(s) with the requested visibility.
    let moved_text = selected
        .iter()
        .map(|info| render_java_static_final_with_visibility(info, &visibility))
        .collect::<Vec<_>>()
        .join("\n");

    // Source-side edits: either remove the declaration or rewrite its
    // visibility (when keep_copy is true and current visibility is tighter
    // than `package`).
    let mut source_edits = Vec::new();
    for info in &selected {
        if keep_copy {
            if let Some(edit) = widen_static_final_visibility_edit(info, &source_parsed.source) {
                source_edits.push(edit);
            }
        } else {
            // Use leading_trivia_start..(end-of-line-after-byte_end) so back-to-back
            // declarations produce adjacent (not overlapping) edits — trailing_trivia_end
            // greedily consumes the next line's indentation and would overlap the
            // following declaration's leading_trivia_start.
            let end = end_of_line_after(&source_parsed.source, info.field.item.byte_end);
            source_edits.push(TextEdit {
                byte_start: info.field.item.leading_trivia_start,
                byte_end: end,
                replacement: String::new(),
            });
        }
    }
    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Target file: create-if-missing, mirroring extract_java_methods.
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    let target_content = if !original_target_bytes.is_empty() {
        let target_parsed = parse_source_file(&target_path)?;
        if target_parsed.language != "java" {
            bail!("move_java_constant only supports java files");
        }
        let target_class = find_first_class_declaration(target_parsed.tree.root_node())
            .ok_or_else(|| anyhow!("no class declaration found in {}", target_path.display()))?;
        let insert_at = java_after_fields_insert_position(target_class, &target_parsed.source);
        let mut text = target_parsed.source.clone();
        text.insert_str(insert_at, &format!("\n{}", moved_text));
        text
    } else {
        let class_name = java_target_type_name(p, &target_path)?;
        let resolved_pkg =
            resolve_java_target_package(p, &source_parsed.source, &source_path, &target_path)?;
        let prelude =
            java_default_target_prelude(p, &source_parsed.source, resolved_pkg.as_deref());
        // Indent constant declarations to match class-body conventions.
        let body = moved_text
            .lines()
            .map(|line| {
                if line.is_empty() {
                    line.to_string()
                } else {
                    format!("    {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        java_class_wrapper(&class_name, &prelude, &body)
    };

    let mut edits = Vec::new();
    if !source_edits.is_empty() {
        edits.push(FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source_parsed.source.as_bytes()),
            edits: source_edits,
            new_text: None,
        });
    }
    edits.push(FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&original_target_bytes),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: original_target_bytes.len(),
            replacement: target_content,
        }],
        new_text: None,
    });

    let plan = RefactorPlan {
        title: format!(
            "Move {} Java constant(s) from {} to {}",
            selected.len(),
            source_path.display(),
            target_path.display()
        ),
        kind: "move_java_constant".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits,
        validations: parse_validation_step_for_path(&source_path)
            .into_iter()
            .chain(parse_validation_step_for_path(&target_path))
            .collect(),
        items: selected.into_iter().map(|info| info.field.item).collect(),
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
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_update_java_callers(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("update_java_callers only supports java files");
    }
    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required for update_java_callers"))?;
    validate_java_member_name(delegate_field, "delegate_field")?;
    let methods = p
        .item_names
        .as_deref()
        .filter(|methods| !methods.is_empty())
        .ok_or_else(|| anyhow!("item_names (method names) is required for update_java_callers"))?;
    let edits = java_caller_rewrite_edits(&parsed, methods, delegate_field, &[])?;
    if edits.is_empty() {
        bail!("no matching Java call sites found");
    }
    let plan = RefactorPlan {
        title: format!(
            "Rewrite {} Java call site(s) through {} in {}",
            edits.len(),
            delegate_field,
            source_path.display()
        ),
        kind: "update_java_callers".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
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
    Ok(serde_json::to_string_pretty(&plan)?)
}

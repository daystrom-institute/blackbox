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
        analyze_extracted_dependencies(&parsed, &selected, p.project_dir.as_deref().map(Path::new))
    } else {
        Default::default()
    };

    // Gap 17: detect cross-class instance-method moves before sorting
    // (we need the original byte_starts to map back to method_declaration
    // nodes via find_node). When ANY selected method is instance-level
    // (not static) AND the target file's stem differs from the source
    // class name, emit a structured advisory in the plan response so
    // the operator knows callers via the old receiver type will break.
    let cross_class_advisory =
        compute_cross_class_instance_move_advisory(&parsed, &selected, &source_path, &target_path);

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

    // Gap 17: when a cross-class instance-method move was detected,
    // attach the advisory to the response body. The standard plan
    // shape doesn't have a slot for this, so we serialize the plan,
    // re-parse as a Value, splice the field in, and re-serialize.
    // Keeps RefactorPlan struct unchanged.
    if let Some(advisory) = cross_class_advisory {
        let mut value = serde_json::to_value(&plan)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "cross_class_instance_move_advisory".to_string(),
                serde_json::to_value(&advisory)?,
            );
        }
        return Ok(serde_json::to_string_pretty(&value)?);
    }
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Gap 17: instance-method cross-class move advisory shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrossClassInstanceMoveAdvisory {
    code: String,
    source_class: String,
    target_class_simple_name: String,
    instance_methods: Vec<String>,
    message: String,
}

/// Gap 17: returns a populated advisory when ANY selected method is
/// instance-level (no `static` modifier) AND the target file path's
/// stem differs from the source class name. The advisory tells the
/// operator that callers holding a reference of the source-class type
/// will fail to compile after the move, and points them at
/// `find_java_usages` to enumerate the breakage scope.
fn compute_cross_class_instance_move_advisory(
    parsed: &ParsedSource,
    selected: &[JavaMethod],
    source_path: &Path,
    target_path: &Path,
) -> Option<CrossClassInstanceMoveAdvisory> {
    // Identify the enclosing class for each selected method, and
    // collect the instance-level ones.
    let source_class_name = find_first_class_declaration(parsed.tree.root_node())
        .map(|n| java_class_name(n, &parsed.source))?;
    let target_simple = target_path
        .file_stem()
        .and_then(|s| s.to_str())?
        .to_string();
    if source_class_name == target_simple {
        // Same simple name on both sides (sibling-class style move
        // within an inheritance pair, or just a target with the same
        // file stem). Operator knows what they're doing.
        return None;
    }
    let _ = source_path; // currently only used as anchor for the parse

    let mut instance_methods: Vec<String> = Vec::new();
    for method in selected {
        let Some(name) = method.item.name.as_deref() else {
            continue;
        };
        let node = find_node(parsed.tree.root_node(), |n: Node<'_>| {
            n.kind() == "method_declaration"
                && n.start_byte() == method.item.byte_start
                && n.end_byte() == method.item.byte_end
        });
        let Some(node) = node else {
            continue;
        };
        if !method_is_static(node) {
            instance_methods.push(name.to_string());
        }
    }
    if instance_methods.is_empty() {
        return None;
    }
    Some(CrossClassInstanceMoveAdvisory {
        code: "cross_class_instance_method_move".to_string(),
        source_class: source_class_name.clone(),
        target_class_simple_name: target_simple.clone(),
        instance_methods: instance_methods.clone(),
        message: format!(
            "Instance method(s) {names} are being moved from `{source_class_name}` to a \
             different class `{target_simple}`. Callers that hold a reference of type \
             `{source_class_name}` and invoke the method via `<instance>.<method>(...)` \
             will fail to compile after the move — `extract_java_methods` does NOT \
             rewrite their receiver type. Either: (a) make the method `static` first \
             and rely on the cross-file static caller rewrite (Gap 4), (b) leave a \
             thin forwarder on `{source_class_name}` that delegates to \
             `{target_simple}`, or (c) run `bbox_refactor_plan(kind=\"find_java_usages\", \
             item_names=[<method>])` to enumerate cross-file callers and rewire them \
             manually after apply.",
            names = instance_methods
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    })
}

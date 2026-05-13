use super::*;

#[derive(Debug, Clone)]
pub(crate) struct RustImplMethod {
    pub(crate) impl_name: String,
    pub(crate) impl_byte_start: usize,
    pub(crate) item: SyntaxItem,
}

#[derive(Debug, Clone)]
pub(crate) struct RustStructField {
    pub(crate) name_byte_start: usize,
    pub(crate) item: SyntaxItem,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TargetImplInsertion {
    byte: usize,
    body_is_empty: bool,
}

pub(crate) fn plan_extract_rust_items(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_items"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    let selected_ids: HashSet<_> = selected
        .iter()
        .map(|item| item.plan_local_id.clone())
        .collect();
    let leftovers = items
        .iter()
        .filter(|item| !selected_ids.contains(&item.plan_local_id))
        .map(|item| {
            format!(
                "{} {} bytes {}..{}",
                item.kind,
                item.name.as_deref().unwrap_or("(unnamed)"),
                item.byte_start,
                item.byte_end
            )
        })
        .collect::<Vec<_>>();

    let mut moved = String::new();
    for item in &selected {
        let text = parsed
            .source
            .get(item.leading_trivia_start..item.byte_end)
            .ok_or_else(|| anyhow!("invalid item range for {}", item.plan_local_id))?
            .trim_matches('\n');
        if !moved.is_empty() {
            moved.push_str("\n\n");
        }
        moved.push_str(text);
        moved.push('\n');
    }

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let moved_insert = if target_source.trim().is_empty() {
        moved
    } else {
        format!(
            "{}{}",
            if target_source.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            },
            moved
        )
    };
    let target_prelude = p
        .target_prelude
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| !rust_prelude_present(&target_source, text));

    let source_edits = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let mut target_edits = Vec::new();
    if let Some(prelude) = target_prelude {
        if target_source.trim().is_empty() {
            target_edits.push(TextEdit {
                byte_start: 0,
                byte_end: 0,
                replacement: format!("{prelude}\n\n{moved_insert}"),
            });
        } else {
            let prelude_insert = rust_prelude_insert_byte(&target_source);
            target_edits.push(TextEdit {
                byte_start: prelude_insert,
                byte_end: prelude_insert,
                replacement: format!("{prelude}\n"),
            });
            if !moved_insert.is_empty() {
                target_edits.push(TextEdit {
                    byte_start: target_source.len(),
                    byte_end: target_source.len(),
                    replacement: moved_insert,
                });
            }
        }
    } else if !moved_insert.is_empty() {
        target_edits.push(TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: moved_insert,
        });
    }
    ensure_non_overlapping(&target_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "extract {} Rust item(s) from {} to {}",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "extract_rust_items".to_string(),
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
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_source.as_bytes()),
                edits: target_edits,
                new_text: None,
            },
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
        items: selected.into_iter().cloned().collect(),
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

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_rust_impl_methods(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_impl_methods"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if let Some(router_name) = p.router_name.as_deref() {
        validate_rust_identifier(router_name, "router_name")?;
    }
    if let Some(export_name) = p.router_export_name.as_deref() {
        validate_rust_identifier(export_name, "router_export_name")?;
        if p.router_name.is_none() {
            bail!("router_export_name requires router_name");
        }
    }
    if let Some(kinds) = p.item_kinds.as_deref() {
        if !kinds.iter().all(|kind| kind == "impl_method") {
            bail!("extract_rust_impl_methods only supports item_kinds impl_method");
        }
    }

    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for extract_rust_impl_methods"))?;

    let parsed = parse_rust_file(&source_path)?;
    let methods = rust_impl_methods(&parsed);
    let candidates = methods
        .iter()
        .filter(|method| {
            p.impl_name
                .as_deref()
                .is_none_or(|impl_name| method.impl_name == impl_name)
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        if let Some(impl_name) = p.impl_name.as_deref() {
            bail!("no impl block matching `{impl_name}` found");
        }
        bail!("no Rust impl methods found");
    }

    let mut selected: Vec<RustImplMethod> = Vec::new();
    for expected in names {
        let matches = candidates
            .iter()
            .copied()
            .filter(|method| method.item.name.as_deref() == Some(expected.as_str()))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested impl method `{expected}` was not found"),
            [method] => selected.push((**method).clone()),
            _ => bail!(
                "requested impl method `{expected}` matched multiple impl blocks; pass impl_name"
            ),
        }
    }

    let impl_starts = selected
        .iter()
        .map(|method| method.impl_byte_start)
        .collect::<HashSet<_>>();
    if impl_starts.len() > 1 {
        bail!("extract_rust_impl_methods can only extract methods from one impl block per plan");
    }

    let selected_ids = selected
        .iter()
        .map(|method| method.item.plan_local_id.clone())
        .collect::<HashSet<_>>();
    let leftovers = methods
        .iter()
        .filter(|method| !selected_ids.contains(&method.item.plan_local_id))
        .map(|method| {
            format!(
                "impl_method {} in {} bytes {}..{}",
                method.item.name.as_deref().unwrap_or("(unnamed)"),
                method.impl_name,
                method.item.byte_start,
                method.item.byte_end
            )
        })
        .collect::<Vec<_>>();

    let source_edits = selected
        .iter()
        .map(|method| TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let parent_after_move = apply_text_edits(&parsed.source, &source_edits)?;
    let parent_still_calls_moved_method = selected
        .iter()
        .filter_map(|method| method.item.name.as_deref())
        .any(|name| rust_text_contains_identifier(&parent_after_move, name));
    let effective_visibility = p
        .visibility
        .as_deref()
        .or_else(|| parent_still_calls_moved_method.then_some("pub(super)"));
    let rebase_super_paths = rust_target_is_child_module_of_source(&source_path, &target_path);

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let target_edits = rust_impl_methods_target_edits(
        &target_path,
        &target_source,
        p.target_prelude.as_deref(),
        p.router_name.as_deref(),
        p.router_export_name.as_deref(),
        &selected[0].impl_name,
        &parsed.source,
        &selected,
        effective_visibility,
        rebase_super_paths,
    )?;

    // RX-A1: run deep analysis before consuming `selected`.
    let (semantic_status, deep_analysis) = if p.deep_analysis == Some(true) {
        let method_name_strs: Vec<&str> = selected
            .iter()
            .filter_map(|m| m.item.name.as_deref())
            .collect();
        let impl_name = &selected[0].impl_name;
        match super::rust_deep::deep_analyze_extract(&source_path, impl_name, &method_name_strs) {
            Ok(da) => (SemanticStatus::IndexedHints, Some(da)),
            Err(e) => {
                eprintln!("deep_analyze_extract warning: {e}");
                (SemanticStatus::SyntaxOnly, None)
            }
        }
    } else {
        (SemanticStatus::SyntaxOnly, None)
    };

    // RX-A2: generate FIXME markers when deep analysis found blocking dependencies.
    let (target_new_text, plan_status, fixme_count) = if let Some(da) = &deep_analysis {
        let (markers, count) = super::rust_deep::generate_fixme_markers(da);
        if count > 0 {
            let applied = apply_text_edits(&target_source, &target_edits)?;
            let with_markers = format!("{markers}\n{applied}");
            (
                Some(with_markers),
                PlanStatus::Blocked,
                Some(FixmeCount {
                    plan_only: count,
                    warning: 0,
                }),
            )
        } else {
            (None, PlanStatus::Planned, None)
        }
    } else {
        (None, PlanStatus::Planned, None)
    };

    let plan = RefactorPlan {
        title: format!(
            "extract {} Rust impl method(s) from {} to {}",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "extract_rust_impl_methods".to_string(),
        semantic_status,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
                new_text: None,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_source.as_bytes()),
                edits: target_edits,
                new_text: target_new_text,
            },
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
        items: selected.into_iter().map(|method| method.item).collect(),
        leftovers,
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis,
        plan_status,
        fixme_count,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_delete_rust_items(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let has_names = p
        .item_names
        .as_deref()
        .is_some_and(|names| !names.is_empty());
    if !has_names {
        bail!(
            "delete_rust_items requires non-empty item_names; use item_kinds only to narrow deletion matches"
        );
    }
    let wants_impl_methods = p
        .item_kinds
        .as_deref()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "impl_method"));
    if wants_impl_methods {
        plan_delete_rust_impl_methods(p, &source_path)
    } else {
        plan_delete_rust_top_level_items(p, &source_path)
    }
}

pub(crate) fn plan_delete_rust_top_level_items(
    p: &RefactorPlanParams,
    source_path: &Path,
) -> Result<String> {
    if let Some(kinds) = p.item_kinds.as_deref() {
        if kinds.iter().any(|kind| kind == "impl_method") {
            bail!("delete_rust_items cannot mix impl_method with top-level item kinds");
        }
    }
    let parsed = parse_rust_file(source_path)?;
    let items = rust_items(&parsed);
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    build_delete_rust_plan(&parsed, "top-level Rust item(s)", &items, selected)
}

pub(crate) fn plan_delete_rust_impl_methods(
    p: &RefactorPlanParams,
    source_path: &Path,
) -> Result<String> {
    if let Some(kinds) = p.item_kinds.as_deref() {
        if !kinds.iter().all(|kind| kind == "impl_method") {
            bail!("delete_rust_items cannot mix impl_method with top-level item kinds");
        }
    }
    let parsed = parse_rust_file(source_path)?;
    let methods = rust_impl_methods(&parsed);
    let candidates = methods
        .iter()
        .filter(|method| {
            p.impl_name
                .as_deref()
                .is_none_or(|impl_name| method.impl_name == impl_name)
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        if let Some(impl_name) = p.impl_name.as_deref() {
            bail!("no impl block matching `{impl_name}` found");
        }
        bail!("no Rust impl methods found");
    }

    let items = candidates
        .iter()
        .map(|method| method.item.clone())
        .collect::<Vec<_>>();
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    if p.impl_name.is_none() {
        if let Some(names) = p.item_names.as_deref() {
            for name in names {
                let matches = selected
                    .iter()
                    .filter(|item| item.name.as_deref() == Some(name.as_str()))
                    .count();
                if matches > 1 {
                    bail!(
                        "requested impl method `{name}` matched multiple impl blocks; pass impl_name"
                    );
                }
            }
        }
    }
    build_delete_rust_plan(&parsed, "Rust impl method(s)", &items, selected)
}

pub(crate) fn build_delete_rust_plan(
    parsed: &ParsedSource,
    label: &str,
    all_items: &[SyntaxItem],
    selected: Vec<&SyntaxItem>,
) -> Result<String> {
    let selected_ids: HashSet<_> = selected
        .iter()
        .map(|item| item.plan_local_id.clone())
        .collect();
    let leftovers = all_items
        .iter()
        .filter(|item| !selected_ids.contains(&item.plan_local_id))
        .map(|item| {
            format!(
                "{} {} bytes {}..{}",
                item.kind,
                item.name.as_deref().unwrap_or("(unnamed)"),
                item.byte_start,
                item.byte_end
            )
        })
        .collect::<Vec<_>>();
    let source_edits = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "delete {} {} from {}",
            selected.len(),
            label,
            path_string(&parsed.path)
        ),
        kind: "delete_rust_items".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&parsed.path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: source_edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&parsed.path),
            byte_range: None,
        }],
        items: selected.into_iter().cloned().collect(),
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

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_add_rust_router_to_sum(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let router_call = if let Some(router_call) = p.router_call.as_deref() {
        validate_rust_router_call(router_call, "router_call")?;
        router_call.to_string()
    } else {
        let router_name = p
            .router_name
            .as_deref()
            .ok_or_else(|| anyhow!("router_name is required for add_rust_router_to_sum"))?;
        validate_rust_identifier(router_name, "router_name")?;
        format!("Self::{router_name}()")
    };

    let parsed = parse_rust_file(&source_path)?;
    let field = find_rust_field_initializer(&parsed, "tool_router")
        .ok_or_else(|| anyhow!("no `tool_router:` field initializer found"))?;
    let field_text = parsed
        .source
        .get(field.start_byte()..field.end_byte())
        .ok_or_else(|| anyhow!("invalid tool_router field range"))?;
    if field_text.contains(&router_call) {
        bail!("tool_router already contains {router_call}");
    }
    let expr_end = rust_field_value_end(&parsed.source, field)
        .ok_or_else(|| anyhow!("could not locate tool_router expression end"))?;

    let edit = TextEdit {
        byte_start: expr_end,
        byte_end: expr_end,
        replacement: format!(" + {router_call}"),
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust router call {router_call} to tool_router sum in {}",
            path_string(&source_path)
        ),
        kind: "add_rust_router_to_sum".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: Vec::new(),
        leftovers: vec![format!("existing tool_router field: {}", field_text.trim())],
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

pub(crate) fn plan_add_rust_mod_decl(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let module_name = p
        .module_name
        .as_deref()
        .or_else(|| {
            p.item_names
                .as_deref()
                .and_then(|names| names.first().map(String::as_str))
        })
        .ok_or_else(|| anyhow!("module_name is required for add_rust_mod_decl"))?;
    validate_rust_identifier(module_name, "module_name")?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let declaration = format!("{visibility}mod {module_name};");

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    if items
        .iter()
        .any(|item| item.kind == "mod_item" && item.name.as_deref() == Some(module_name))
    {
        bail!("module declaration `{module_name}` already exists");
    }

    let last_mod = items
        .iter()
        .filter(|item| item.kind == "mod_item")
        .filter(|item| ensure_rust_mod_declaration(&parsed.source, item).is_ok())
        .max_by_key(|item| item.byte_end);
    let (insert_at, replacement) = if let Some(item) = last_mod {
        (item.byte_end, format!("\n{declaration}"))
    } else {
        (
            rust_module_decl_fallback_insert_byte(&parsed.source),
            format!("{declaration}\n"),
        )
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust module declaration {declaration} to {}",
            path_string(&source_path)
        ),
        kind: "add_rust_mod_decl".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
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

pub(crate) fn plan_add_rust_use_decl(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let use_path = p
        .use_path
        .as_deref()
        .ok_or_else(|| anyhow!("use_path is required for add_rust_use_decl"))?;
    validate_rust_use_path(use_path)?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let declaration = format!("{visibility}use {use_path};");

    let parsed = parse_rust_file(&source_path)?;
    if parsed.source.lines().any(|line| line.trim() == declaration) {
        bail!("use declaration `{declaration}` already exists");
    }
    let items = rust_items(&parsed);
    let insert_at = items
        .iter()
        .filter(|item| item.kind == "use_declaration")
        .max_by_key(|item| item.byte_end)
        .map(|item| item.byte_end)
        .or_else(|| {
            items
                .iter()
                .filter(|item| item.kind == "mod_item")
                .max_by_key(|item| item.byte_end)
                .map(|item| item.trailing_trivia_end)
        })
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(&parsed.source));
    let replacement = if parsed.source[insert_at..].starts_with('\n') {
        format!("\n{declaration}")
    } else if insert_at == parsed.source.len() || parsed.source[..insert_at].ends_with('\n') {
        format!("{declaration}\n")
    } else {
        format!("\n{declaration}\n")
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust use declaration {declaration} to {}",
            path_string(&source_path)
        ),
        kind: "add_rust_use_decl".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
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

pub(crate) fn plan_copy_rust_mod_decls(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for copy_rust_mod_decls"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let parsed = parse_rust_file(&source_path)?;
    let source_items = rust_items(&parsed);
    let mod_items = source_items
        .iter()
        .filter(|item| item.kind == "mod_item")
        .cloned()
        .collect::<Vec<_>>();
    let mod_kind = vec!["mod_item".to_string()];
    let selected = select_items(&mod_items, p.item_names.as_deref(), Some(&mod_kind))?;
    let mut declarations = Vec::new();
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    for item in &selected {
        ensure_rust_mod_declaration(&parsed.source, item)?;
        let name = item
            .name
            .as_deref()
            .ok_or_else(|| anyhow!("selected mod_item has no module name"))?;
        declarations.push(format!("{visibility}mod {name};"));
    }

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let existing = rust_existing_mod_decl_names(&target_path, &target_source)?;
    let declarations = declarations
        .into_iter()
        .filter(|declaration| {
            let name = declaration
                .trim_end_matches(';')
                .split_whitespace()
                .last()
                .unwrap_or_default();
            !existing.contains(name)
        })
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        bail!("all selected module declarations already exist in target");
    }

    let insert_at = rust_mod_decl_insert_byte(&target_path, &target_source)?;
    let replacement = rust_decl_batch_insert_text(&target_source, insert_at, &declarations);
    let plan = RefactorPlan {
        title: format!(
            "copy {} Rust module declaration(s) from {} to {}",
            declarations.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "copy_rust_mod_decls".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(target_source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&target_path),
            byte_range: None,
        }],
        items: selected.into_iter().cloned().collect(),
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

pub(crate) fn plan_rewrite_rust_mod_visibility(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let module_name = p
        .module_name
        .as_deref()
        .or_else(|| {
            p.item_names
                .as_deref()
                .and_then(|names| names.first().map(String::as_str))
        })
        .ok_or_else(|| {
            anyhow!("module_name or item_names[0] is required for rewrite_rust_mod_visibility")
        })?;
    validate_rust_identifier(module_name, "module_name")?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = items
        .iter()
        .filter(|item| item.kind == "mod_item" && item.name.as_deref() == Some(module_name))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("module declaration `{module_name}` was not found");
    }
    if selected.len() > 1 {
        bail!("module declaration `{module_name}` matched multiple items");
    }
    let item = selected[0];
    ensure_rust_mod_declaration(&parsed.source, item)?;
    let mod_keyword = rust_mod_keyword_byte(&parsed.source, item)?;
    let visibility_start = rust_mod_visibility_start_byte(&parsed.source, mod_keyword);
    let current_prefix = &parsed.source[visibility_start..mod_keyword];
    if current_prefix == visibility {
        bail!("module declaration `{module_name}` already has requested visibility");
    }
    let plan = RefactorPlan {
        title: format!(
            "rewrite Rust module declaration {module_name} visibility in {}",
            path_string(&source_path)
        ),
        kind: "rewrite_rust_mod_visibility".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: visibility_start,
                byte_end: mod_keyword,
                replacement: visibility.to_string(),
            }],
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: vec![item.clone()],
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

pub(crate) fn plan_rewrite_rust_item_visibility(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let parsed = parse_rust_file(&source_path)?;
    let wants_impl_methods = p
        .item_kinds
        .as_deref()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "impl_method"));
    let items = if wants_impl_methods {
        if let Some(kinds) = p.item_kinds.as_deref() {
            if !kinds.iter().all(|kind| kind == "impl_method") {
                bail!(
                    "rewrite_rust_item_visibility cannot mix impl_method with top-level item kinds"
                );
            }
        }
        let methods = rust_impl_methods(&parsed);
        let candidates = methods
            .iter()
            .filter(|method| {
                p.impl_name
                    .as_deref()
                    .is_none_or(|impl_name| method.impl_name == impl_name)
            })
            .map(|method| method.item.clone())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            if let Some(impl_name) = p.impl_name.as_deref() {
                bail!("no impl block matching `{impl_name}` found");
            }
            bail!("no Rust impl methods found");
        }
        candidates
    } else {
        if p.impl_name.is_some() {
            bail!("impl_name requires item_kinds=[\"impl_method\"]");
        }
        rust_items(&parsed)
    };
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    if wants_impl_methods && p.impl_name.is_none() {
        if let Some(names) = p.item_names.as_deref() {
            for name in names {
                let matches = selected
                    .iter()
                    .filter(|item| item.name.as_deref() == Some(name.as_str()))
                    .count();
                if matches > 1 {
                    bail!(
                        "requested impl method `{name}` matched multiple impl blocks; pass impl_name"
                    );
                }
            }
        }
    }

    let mut edits = Vec::new();
    for item in &selected {
        let keyword = rust_visibility_keyword_byte(&parsed.source, item)?;
        let visibility_start = rust_item_visibility_start_byte(&parsed.source, item, keyword);
        let current_prefix = &parsed.source[visibility_start..keyword];
        let qualifier_prefix = rust_strip_visibility_prefix(current_prefix);
        let replacement = format!("{visibility}{qualifier_prefix}");
        if current_prefix == replacement {
            continue;
        }
        edits.push(TextEdit {
            byte_start: visibility_start,
            byte_end: keyword,
            replacement,
        });
    }
    if edits.is_empty() {
        bail!("all selected Rust items already have requested visibility");
    }
    ensure_non_overlapping(&edits)?;
    let plan = RefactorPlan {
        title: format!(
            "rewrite visibility for {} Rust item(s) in {}",
            selected.len(),
            path_string(&source_path)
        ),
        kind: "rewrite_rust_item_visibility".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: selected.into_iter().cloned().collect(),
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

pub(crate) fn plan_rewrite_rust_field_visibility(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let parsed = parse_rust_file(&source_path)?;
    let struct_names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names must name one or more Rust structs"))?;
    let struct_name_set = struct_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let struct_items = rust_items(&parsed)
        .into_iter()
        .filter(|item| {
            item.kind == "struct_item"
                && item
                    .name
                    .as_deref()
                    .is_some_and(|name| struct_name_set.contains(name))
        })
        .collect::<Vec<_>>();
    if struct_items.is_empty() {
        bail!("no matching Rust structs found");
    }
    for name in struct_names {
        if !struct_items
            .iter()
            .any(|item| item.name.as_deref() == Some(name.as_str()))
        {
            bail!("requested struct `{name}` was not found");
        }
    }

    let mut fields = Vec::new();
    for struct_item in &struct_items {
        fields.extend(rust_named_struct_fields(&parsed, struct_item)?);
    }
    if fields.is_empty() {
        bail!("no named struct fields found");
    }

    let mut edits = Vec::new();
    for field in &fields {
        let visibility_start =
            rust_item_visibility_start_byte(&parsed.source, &field.item, field.name_byte_start);
        let current_prefix = &parsed.source[visibility_start..field.name_byte_start];
        if current_prefix == visibility {
            continue;
        }
        edits.push(TextEdit {
            byte_start: visibility_start,
            byte_end: field.name_byte_start,
            replacement: visibility.to_string(),
        });
    }
    if edits.is_empty() {
        bail!("all selected Rust fields already have requested visibility");
    }
    ensure_non_overlapping(&edits)?;
    let plan = RefactorPlan {
        title: format!(
            "rewrite visibility for {} Rust struct field(s) in {}",
            fields.len(),
            path_string(&source_path)
        ),
        kind: "rewrite_rust_field_visibility".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: fields.into_iter().map(|field| field.item).collect(),
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

pub(crate) fn plan_rust_lsp_rename(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let project_dir = p
        .project_dir
        .as_deref()
        .map(|dir| resolve_path(None, dir))
        .transpose()?
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(&source_path)
                .unwrap_or_else(|| source_path.parent().unwrap_or(Path::new(".")).to_path_buf())
        });
    let old_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .or(p.old_text.as_deref())
        .ok_or_else(|| anyhow!("item_names[0] or old_text is required for rust_lsp_rename"))?;
    validate_rust_identifier(old_name, "item_names[0]")?;
    let new_name = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for rust_lsp_rename"))?;
    validate_rust_identifier(new_name, "new_text")?;
    if old_name == new_name {
        bail!("rust_lsp_rename requires different old and new names");
    }
    let parsed = parse_rust_file(&source_path)?;
    let position_byte = rust_rename_position_byte(&parsed, old_name)?;
    let position = byte_to_lsp_position(&parsed.source, position_byte);
    let manager = ctx
        .lsp
        .as_ref()
        .ok_or_else(|| anyhow!("rust_lsp_rename requires the LSP session manager"))?;
    let file_edits = rust_analyzer_rename(manager, &project_dir, &source_path, position, new_name)?;
    if file_edits.is_empty() {
        bail!("rust-analyzer returned no edits for rename `{old_name}`");
    }
    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();
    let plan = RefactorPlan {
        title: format!("rust-analyzer rename {old_name} to {new_name}"),
        kind: "rust_lsp_rename".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
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

pub(crate) fn plan_rust_organize_imports(
    p: &RefactorPlanParams,
    ctx: &PlanContext,
) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let project_dir = p
        .project_dir
        .as_deref()
        .map(|dir| resolve_path(None, dir))
        .transpose()?
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(&source_path)
                .unwrap_or_else(|| source_path.parent().unwrap_or(Path::new(".")).to_path_buf())
        });
    parse_rust_file(&source_path)?;
    let manager = ctx
        .lsp
        .as_ref()
        .ok_or_else(|| anyhow!("rust_organize_imports requires the LSP session manager"))?;
    let file_edits = rust_analyzer_organize_imports(manager, &project_dir, &source_path)?;
    if file_edits.is_empty() {
        bail!("rust-analyzer returned no import organization edits");
    }
    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();
    let plan = RefactorPlan {
        title: format!(
            "rust-analyzer organize imports in {}",
            path_string(&source_path)
        ),
        kind: "rust_organize_imports".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
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

pub(crate) fn rust_impl_methods_target_edits(
    target_path: &Path,
    target_source: &str,
    target_prelude: Option<&str>,
    router_name: Option<&str>,
    router_export_name: Option<&str>,
    impl_name: &str,
    source: &str,
    selected: &[RustImplMethod],
    visibility: Option<&str>,
    rebase_super_paths: bool,
) -> Result<Vec<TextEdit>> {
    if let Some(insertion) =
        existing_target_impl_insert_byte(target_path, target_source, impl_name, router_name)?
    {
        let mut replacement = String::new();
        if !insertion.body_is_empty {
            replacement.push('\n');
        }
        replacement.push_str(&rust_impl_methods_block(
            source,
            selected,
            visibility,
            rebase_super_paths,
        )?);
        return Ok(vec![TextEdit {
            byte_start: insertion.byte,
            byte_end: insertion.byte,
            replacement,
        }]);
    }

    let wrapper = rust_impl_methods_target_wrapper(
        target_source,
        router_name,
        router_export_name,
        impl_name,
        source,
        selected,
        visibility,
        rebase_super_paths,
    )?;
    let Some(prelude) = target_prelude
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| !rust_prelude_present(target_source, text))
    else {
        return Ok(vec![TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: wrapper,
        }]);
    };

    if target_source.trim().is_empty() {
        return Ok(vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: format!("{prelude}\n\n{wrapper}"),
        }]);
    }

    let prelude_insert = rust_prelude_insert_byte(target_source);
    if prelude_insert == target_source.len() {
        return Ok(vec![TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: format!("{prelude}\n\n{wrapper}"),
        }]);
    }

    Ok(vec![
        TextEdit {
            byte_start: prelude_insert,
            byte_end: prelude_insert,
            replacement: format!("{prelude}\n\n"),
        },
        TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: wrapper,
        },
    ])
}

pub(crate) fn find_rust_field_initializer<'a>(
    parsed: &'a ParsedSource,
    field_name: &str,
) -> Option<Node<'a>> {
    find_node(parsed.tree.root_node(), |node| {
        node.kind() == "field_initializer"
            && rust_field_initializer_name(node, &parsed.source).as_deref() == Some(field_name)
    })
}

pub(crate) fn rust_field_initializer_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("field"))
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
        .or_else(|| {
            node.utf8_text(source.as_bytes())
                .ok()
                .and_then(|text| text.split(':').next())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
}

pub(crate) fn rust_field_value_end(source: &str, node: Node<'_>) -> Option<usize> {
    if let Some(value) = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("body"))
    {
        return Some(value.end_byte());
    }
    let text = source.get(node.start_byte()..node.end_byte())?;
    let colon = text.find(':')?;
    let mut end = node.end_byte();
    let value = &text[colon + 1..];
    let trimmed_len = value.trim_end().len();
    end -= value.len().saturating_sub(trimmed_len);
    Some(end)
}

pub(crate) fn rust_prelude_present(target_source: &str, prelude: &str) -> bool {
    let prelude = prelude.trim();
    if prelude.contains('\n') {
        return target_source.contains(prelude);
    }
    target_source.lines().any(|line| line.trim() == prelude)
}

pub(crate) fn rust_prelude_insert_byte(target_source: &str) -> usize {
    let bytes = target_source.as_bytes();
    let mut idx = 0usize;
    while idx < target_source.len() {
        let line_end = target_source[idx..]
            .find('\n')
            .map(|offset| idx + offset + 1)
            .unwrap_or(target_source.len());
        let line = &target_source[idx..line_end];
        let trimmed = line.trim();
        let is_shebang = idx == 0 && trimmed.starts_with("#!") && !trimmed.starts_with("#![");
        if trimmed.starts_with("/*!") {
            let Some(close) = target_source[idx..].find("*/") else {
                return target_source.len();
            };
            idx += close + 2;
            while idx < bytes.len() && matches!(bytes[idx], b'\n' | b'\r') {
                idx += 1;
            }
            continue;
        }
        let is_inner = trimmed.starts_with("#![") || trimmed.starts_with("//!");
        if trimmed.is_empty() || is_shebang || is_inner {
            idx = line_end;
            continue;
        }
        break;
    }
    while idx < bytes.len() && matches!(bytes[idx], b'\n' | b'\r') {
        idx += 1;
    }
    idx
}

pub(crate) fn rust_module_decl_fallback_insert_byte(source: &str) -> usize {
    rust_prelude_insert_byte(source)
}

pub(crate) fn rust_impl_methods_target_wrapper(
    target_source: &str,
    router_name: Option<&str>,
    router_export_name: Option<&str>,
    impl_name: &str,
    source: &str,
    selected: &[RustImplMethod],
    visibility: Option<&str>,
    rebase_super_paths: bool,
) -> Result<String> {
    let mut wrapper = String::new();
    if let Some(export_name) = router_export_name {
        let router_name =
            router_name.ok_or_else(|| anyhow!("router_export_name requires router_name"))?;
        let vis_str = visibility
            .map(|v| format!("{v} "))
            .unwrap_or_else(|| "pub(super) ".to_string());
        wrapper.push_str(&vis_str);
        wrapper.push_str("fn ");
        wrapper.push_str(export_name);
        wrapper.push_str("() -> ToolRouter<BlackboxServer> {\n    BlackboxServer::");
        wrapper.push_str(router_name);
        wrapper.push_str("()\n}\n\n");
    }
    if let Some(router_name) = router_name {
        wrapper.push_str("#[tool_router(router = ");
        wrapper.push_str(router_name);
        wrapper.push_str(")]\n");
    }
    wrapper.push_str(impl_name);
    wrapper.push_str(" {\n");
    wrapper.push_str(&rust_impl_methods_block(
        source,
        selected,
        visibility,
        rebase_super_paths,
    )?);
    wrapper.push_str("}\n");

    if target_source.trim().is_empty() {
        Ok(wrapper)
    } else {
        Ok(format!(
            "{}{}",
            if target_source.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            },
            wrapper
        ))
    }
}

pub(crate) fn rust_impl_methods_block(
    source: &str,
    selected: &[RustImplMethod],
    visibility: Option<&str>,
    rebase_super_paths: bool,
) -> Result<String> {
    let mut block = String::new();
    let vis_prefix = if let Some(v) = visibility {
        Some(rust_decl_visibility_prefix(Some(v))?)
    } else {
        None
    };

    for (idx, method) in selected.iter().enumerate() {
        let original_text = source
            .get(method.item.leading_trivia_start..method.item.byte_end)
            .ok_or_else(|| {
                anyhow!(
                    "invalid impl method range for {}",
                    method.item.plan_local_id
                )
            })?;

        let mut text = if let Some(ref new_vis) = vis_prefix {
            let keyword = rust_visibility_keyword_byte(source, &method.item)?;
            let vis_start = rust_item_visibility_start_byte(source, &method.item, keyword);

            let before = source
                .get(method.item.leading_trivia_start..vis_start)
                .unwrap_or_default();

            let current_prefix = source.get(vis_start..keyword).unwrap_or_default();
            let qualifier_prefix = rust_strip_visibility_prefix(current_prefix);

            let after = source
                .get(keyword..method.item.byte_end)
                .unwrap_or_default();

            format!("{before}{new_vis}{qualifier_prefix}{after}")
        } else {
            original_text.to_string()
        };
        if rebase_super_paths {
            text = rust_rebase_super_paths_one_level_deeper(&text);
        }

        if idx > 0 {
            block.push('\n');
        }
        block.push_str(text.trim_matches('\n'));
        block.push('\n');
    }
    Ok(block)
}

fn rust_target_is_child_module_of_source(source_path: &Path, target_path: &Path) -> bool {
    let Some(source_parent) = source_path.parent() else {
        return false;
    };
    let Some(source_stem) = source_path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    target_path.parent() == Some(&source_parent.join(source_stem))
}

fn rust_text_contains_identifier(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(idx, window)| {
            window == needle
                && rust_identifier_boundary(bytes.get(idx.wrapping_sub(1)).copied())
                && rust_identifier_boundary(bytes.get(idx + needle.len()).copied())
        })
}

fn rust_identifier_boundary(ch: Option<u8>) -> bool {
    !matches!(ch, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

fn rust_rebase_super_paths_one_level_deeper(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while idx < text.len() {
        let rest = &text[idx..];
        if rest.starts_with("super::")
            && rust_identifier_boundary(text.as_bytes().get(idx.wrapping_sub(1)).copied())
        {
            out.push_str("super::super::");
            idx += "super::".len();
        } else {
            let ch = rest.chars().next().expect("idx is on char boundary");
            out.push(ch);
            idx += ch.len_utf8();
        }
    }
    out
}

pub(crate) fn validate_rust_identifier(value: &str, field: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("{field} must be a Rust identifier");
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        bail!("{field} must be a Rust identifier");
    }
    Ok(())
}

pub(crate) fn validate_rust_router_call(value: &str, field: &str) -> Result<()> {
    if value.trim() != value {
        bail!("{field} must not have leading or trailing whitespace");
    }
    let Some(path) = value.strip_suffix("()") else {
        bail!("{field} must be a zero-argument Rust path call");
    };
    if path.is_empty() {
        bail!("{field} must be a zero-argument Rust path call");
    }
    for segment in path.split("::") {
        validate_rust_identifier(segment, field)?;
    }
    Ok(())
}

pub(crate) fn rust_decl_visibility_prefix(visibility: Option<&str>) -> Result<&'static str> {
    match visibility.unwrap_or("").trim() {
        "" | "private" => Ok(""),
        "pub" => Ok("pub "),
        "pub(crate)" => Ok("pub(crate) "),
        "pub(super)" => Ok("pub(super) "),
        other => {
            bail!("unsupported Rust visibility `{other}`; supported: pub, pub(crate), pub(super)")
        }
    }
}

pub(crate) fn rust_strip_visibility_prefix(prefix: &str) -> &str {
    for visibility in ["pub ", "pub(crate) ", "pub(super) "] {
        if let Some(rest) = prefix.strip_prefix(visibility) {
            return rest;
        }
    }
    if let Some(rest) = prefix.strip_prefix("pub(") {
        if let Some(close) = rest.find(')') {
            let after_close = close + 1;
            if rest[after_close..].starts_with(' ') {
                return &rest[after_close + 1..];
            }
        }
    }
    prefix
}

pub(crate) fn rust_visibility_keyword_byte(source: &str, item: &SyntaxItem) -> Result<usize> {
    let keyword = match item.kind.as_str() {
        "function_item" | "impl_method" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "const_item" => "const",
        "static_item" => "static",
        "type_item" => "type",
        "mod_item" => "mod",
        other => bail!("visibility rewrite does not support Rust item kind `{other}`"),
    };
    let text = source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid item range for {}", item.plan_local_id))?;
    for (idx, _) in text.match_indices(keyword) {
        let before = idx
            .checked_sub(1)
            .and_then(|pos| text.as_bytes().get(pos))
            .copied();
        let after = text.as_bytes().get(idx + keyword.len()).copied();
        let before_boundary = before.is_none_or(|byte| !rust_ident_byte(byte));
        let after_boundary = after.is_none_or(|byte| !rust_ident_byte(byte));
        if before_boundary && after_boundary {
            return Ok(item.byte_start + idx);
        }
    }
    bail!(
        "could not locate `{keyword}` keyword for {}",
        item.plan_local_id
    )
}

pub(crate) fn rust_visibility_start_byte(source: &str, keyword: usize) -> usize {
    let line_start = line_start_before(source, keyword);
    let leading = source[line_start..keyword]
        .bytes()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    line_start + leading
}

pub(crate) fn rust_item_visibility_start_byte(
    source: &str,
    item: &SyntaxItem,
    keyword: usize,
) -> usize {
    rust_visibility_start_byte(source, keyword).max(item.byte_start)
}

pub(crate) fn rust_mod_keyword_byte(source: &str, item: &SyntaxItem) -> Result<usize> {
    let text = source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid mod_item range for {}", item.plan_local_id))?;
    let name = item
        .name
        .as_deref()
        .ok_or_else(|| anyhow!("selected mod_item has no module name"))?;
    for (idx, _) in text.match_indices("mod") {
        let before = idx
            .checked_sub(1)
            .and_then(|pos| text.as_bytes().get(pos))
            .copied();
        let after = text.as_bytes().get(idx + 3).copied();
        let before_boundary = before.is_none_or(|byte| !rust_ident_byte(byte));
        let after_boundary = after.is_none_or(|byte| !rust_ident_byte(byte));
        if !before_boundary || !after_boundary {
            continue;
        }
        let rest = &text[idx + 3..];
        let trimmed = rest.trim_start();
        if trimmed
            .strip_prefix(name)
            .and_then(|suffix| suffix.as_bytes().first().copied())
            .is_some_and(rust_ident_byte)
        {
            continue;
        }
        if trimmed.starts_with(name) {
            return Ok(item.byte_start + idx);
        }
    }
    bail!("could not locate `mod` keyword for {}", item.plan_local_id)
}

pub(crate) fn rust_mod_visibility_start_byte(source: &str, mod_keyword: usize) -> usize {
    let line_start = line_start_before(source, mod_keyword);
    let leading = source[line_start..mod_keyword]
        .bytes()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    line_start + leading
}

pub(crate) fn rust_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

pub(crate) fn ensure_rust_mod_declaration(source: &str, item: &SyntaxItem) -> Result<()> {
    let text = source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid mod_item range for {}", item.plan_local_id))?;
    let semicolon = text.find(';');
    let brace = text.find('{');
    match (semicolon, brace) {
        (Some(_), None) => Ok(()),
        (Some(semi), Some(brace)) if semi < brace => Ok(()),
        _ => bail!(
            "module `{}` is inline; only `mod name;` declarations are supported",
            item.name.as_deref().unwrap_or("(unnamed)")
        ),
    }
}

pub(crate) fn rust_existing_mod_decl_names(path: &Path, source: &str) -> Result<HashSet<String>> {
    if source.trim().is_empty() {
        return Ok(HashSet::new());
    }
    let tree = parse_source("rust", source)?;
    let parsed = ParsedSource {
        path: path.to_path_buf(),
        language: "rust",
        source: source.to_string(),
        tree,
    };
    let names = rust_items(&parsed)
        .into_iter()
        .filter(|item| item.kind == "mod_item")
        .filter_map(|item| {
            ensure_rust_mod_declaration(source, &item)
                .ok()
                .and(item.name)
        })
        .collect::<HashSet<_>>();
    Ok(names)
}

pub(crate) fn rust_mod_decl_insert_byte(path: &Path, source: &str) -> Result<usize> {
    if source.trim().is_empty() {
        return Ok(0);
    }
    let tree = parse_source("rust", source)?;
    let parsed = ParsedSource {
        path: path.to_path_buf(),
        language: "rust",
        source: source.to_string(),
        tree,
    };
    Ok(rust_items(&parsed)
        .iter()
        .filter(|item| item.kind == "mod_item")
        .filter(|item| ensure_rust_mod_declaration(source, item).is_ok())
        .max_by_key(|item| item.byte_end)
        .map(|item| item.byte_end)
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(source)))
}

pub(crate) fn rust_decl_batch_insert_text(
    source: &str,
    insert_at: usize,
    declarations: &[String],
) -> String {
    let mut text = declarations.join("\n");
    if source.trim().is_empty() || insert_at == 0 {
        text.push('\n');
        text
    } else if source[..insert_at].ends_with('\n') {
        if !source[insert_at..].starts_with('\n') {
            text.push('\n');
        }
        text
    } else {
        format!("\n{text}")
    }
}

pub(crate) fn validate_rust_use_path(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("use_path must not be empty");
    }
    if trimmed != value {
        bail!("use_path must not have leading or trailing whitespace");
    }
    if value.contains('\n') || value.contains('\r') || value.contains(';') {
        bail!("use_path must be a single Rust use path without a trailing semicolon");
    }
    Ok(())
}

pub(crate) fn parse_rust_file(path: &Path) -> Result<ParsedSource> {
    let parsed = parse_source_file(path)?;
    if parsed.language != "rust" {
        bail!("{} is not a Rust source file", path.display());
    }
    Ok(parsed)
}

pub(crate) fn rust_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|node| is_top_level_item(node.kind()))
        .map(|node| syntax_item(parsed, node))
        .collect()
}

pub(crate) fn rust_status_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let mut items = rust_items(parsed);
    items.extend(
        rust_impl_methods(parsed)
            .into_iter()
            .map(|method| method.item),
    );
    items
}

pub(crate) fn rust_impl_methods(parsed: &ParsedSource) -> Vec<RustImplMethod> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|node| node.kind() == "impl_item")
        .flat_map(|impl_node| rust_impl_methods_in(parsed, impl_node))
        .collect()
}

pub(crate) fn rust_impl_methods_in(
    parsed: &ParsedSource,
    impl_node: Node<'_>,
) -> Vec<RustImplMethod> {
    let impl_name = item_name(impl_node, &parsed.source, parsed.language)
        .unwrap_or_else(|| "(unnamed impl)".to_string());
    let mut cursor = impl_node.walk();
    impl_node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "declaration_list")
        .flat_map(|body| {
            let mut body_cursor = body.walk();
            body.named_children(&mut body_cursor)
                .filter(|member| member.kind() == "function_item")
                .map(|member| RustImplMethod {
                    impl_name: impl_name.clone(),
                    impl_byte_start: impl_node.start_byte(),
                    item: syntax_item_with_kind(parsed, member, "impl_method"),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn rust_named_struct_fields(
    parsed: &ParsedSource,
    struct_item: &SyntaxItem,
) -> Result<Vec<RustStructField>> {
    let struct_name = struct_item
        .name
        .clone()
        .ok_or_else(|| anyhow!("selected struct has no name"))?;
    let struct_node = rust_node_by_range(
        parsed.tree.root_node(),
        "struct_item",
        struct_item.byte_start,
        struct_item.byte_end,
    )
    .ok_or_else(|| anyhow!("could not locate tree-sitter node for struct `{struct_name}`"))?;
    let mut fields = Vec::new();
    collect_rust_named_struct_fields(parsed, struct_node, &mut fields)?;
    Ok(fields)
}

pub(crate) fn collect_rust_named_struct_fields(
    parsed: &ParsedSource,
    node: Node<'_>,
    fields: &mut Vec<RustStructField>,
) -> Result<()> {
    if node.kind() == "field_declaration" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let mut item = syntax_item_with_kind(parsed, node, "field_declaration");
            item.name = Some(name_node.utf8_text(parsed.source.as_bytes())?.to_string());
            fields.push(RustStructField {
                name_byte_start: name_node.start_byte(),
                item,
            });
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_rust_named_struct_fields(parsed, child, fields)?;
    }
    Ok(())
}

pub(crate) fn rust_node_by_range<'a>(
    node: Node<'a>,
    kind: &str,
    byte_start: usize,
    byte_end: usize,
) -> Option<Node<'a>> {
    if node.kind() == kind && node.start_byte() == byte_start && node.end_byte() == byte_end {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() > byte_start || child.end_byte() < byte_end {
            continue;
        }
        if let Some(found) = rust_node_by_range(child, kind, byte_start, byte_end) {
            return Some(found);
        }
    }
    None
}

use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionParams, DocumentChanges, Position, Range,
    RenameParams, TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkspaceEdit,
    request::{CodeActionRequest, Rename},
};

use crate::lsp::LspSessionManager;
use crate::projects::Language;

pub(crate) fn rust_rename_position_byte(parsed: &ParsedSource, old_name: &str) -> Result<usize> {
    let mut candidates = rust_status_items(parsed)
        .into_iter()
        .filter(|item| item.name.as_deref() == Some(old_name))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        bail!(
            "no Rust item named `{old_name}` found in {}",
            parsed.path.display()
        );
    }
    candidates.sort_by_key(|item| item.byte_start);
    let item = &candidates[0];
    let item_source = parsed
        .source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid item range for `{old_name}`"))?;
    let relative = item_source
        .find(old_name)
        .ok_or_else(|| anyhow!("could not find `{old_name}` text inside selected item"))?;
    Ok(item.byte_start + relative + old_name.len().saturating_sub(1) / 2)
}

pub(crate) fn byte_to_lsp_position(source: &str, byte: usize) -> Position {
    let line = source[..byte].bytes().filter(|b| *b == b'\n').count() as u32;
    let line_start = line_start_before(source, byte);
    let character = source[line_start..byte].encode_utf16().count() as u32;
    Position { line, character }
}

pub(crate) fn lsp_position_to_byte(source: &str, line: u32, character: u32) -> Result<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (idx, byte) in source.bytes().enumerate() {
        if current_line == line {
            break;
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = idx + 1;
        }
    }
    if current_line != line {
        bail!("line {line} is outside source");
    }
    let line_end = source[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(source.len());
    let mut utf16 = 0u32;
    for (offset, ch) in source[line_start..line_end].char_indices() {
        if utf16 == character {
            return Ok(line_start + offset);
        }
        utf16 += ch.len_utf16() as u32;
        if utf16 > character {
            bail!("character {character} is not on a UTF-16 boundary");
        }
    }
    if utf16 == character {
        return Ok(line_end);
    }
    bail!("character {character} is outside line {line}");
}

pub(crate) fn workspace_edit_to_file_edits(workspace_edit: WorkspaceEdit) -> Result<Vec<FileEdit>> {
    let mut grouped: BTreeMap<PathBuf, Vec<lsp_types::TextEdit>> = BTreeMap::new();

    if let Some(changes) = workspace_edit.changes {
        for (url, edits) in changes {
            if let Ok(path) = url.to_file_path() {
                grouped.entry(path).or_default().extend(edits);
            }
        }
    }

    if let Some(document_changes) = workspace_edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(doc_edits) => {
                for doc_edit in doc_edits {
                    if let Ok(path) = doc_edit.text_document.uri.to_file_path() {
                        let edits = doc_edit.edits.into_iter().map(|e| match e {
                            lsp_types::OneOf::Left(te) => te,
                            lsp_types::OneOf::Right(ate) => lsp_types::TextEdit {
                                range: ate.text_edit.range,
                                new_text: ate.text_edit.new_text,
                            },
                        });
                        grouped.entry(path).or_default().extend(edits);
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(doc_edit) = op {
                        if let Ok(path) = doc_edit.text_document.uri.to_file_path() {
                            let edits = doc_edit.edits.into_iter().map(|e| match e {
                                lsp_types::OneOf::Left(te) => te,
                                lsp_types::OneOf::Right(ate) => lsp_types::TextEdit {
                                    range: ate.text_edit.range,
                                    new_text: ate.text_edit.new_text,
                                },
                            });
                            grouped.entry(path).or_default().extend(edits);
                        }
                    }
                }
            }
        }
    }

    let mut file_edits = Vec::new();
    for (path, edits) in grouped {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read LSP edit target {}", path.display()))?;
        let mut text_edits = Vec::new();
        for edit in edits {
            let byte_start =
                lsp_position_to_byte(&source, edit.range.start.line, edit.range.start.character)
                    .with_context(|| format!("invalid LSP start range for {}", path.display()))?;
            let byte_end =
                lsp_position_to_byte(&source, edit.range.end.line, edit.range.end.character)
                    .with_context(|| format!("invalid LSP end range for {}", path.display()))?;
            text_edits.push(TextEdit {
                byte_start,
                byte_end,
                replacement: edit.new_text,
            });
        }
        file_edits.push(FileEdit {
            path: path_string(&path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: text_edits,
            new_text: None,
        });
    }
    Ok(file_edits)
}

/// Ask rust-analyzer for a workspace rename via the shared session
/// pool. The session is lazily spawned on first call for
/// `(project_dir, Rust)` and reused across subsequent calls; no
/// per-call init/shutdown handshake.
pub(crate) fn rust_analyzer_rename(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    position: Position,
    new_name: &str,
) -> Result<Vec<FileEdit>> {
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let source_text = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let rename_params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        new_name: new_name.to_string(),
        work_done_progress_params: Default::default(),
    };

    let response = manager.with_session(project_dir, Language::Rust, |mut client| {
        client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
            &lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: source_uri.clone(),
                    language_id: "rust".to_string(),
                    version: 0,
                    text: source_text.clone(),
                },
            },
        )?;
        // rust-analyzer indexes lazily on didOpen. Drain until we see
        // diagnostics for this file, otherwise the rename request below
        // arrives before symbol analysis is ready.
        client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
        let id = client.send_request::<Rename>(&rename_params)?;
        client.read_response::<Rename>(id)
    })?;

    if let Some(edit) = response {
        workspace_edit_to_file_edits(edit)
    } else {
        Ok(Vec::new())
    }
}

/// Ask rust-analyzer for `source.organizeImports` code actions on
/// `source_path` using the shared session pool. The session is
/// lazily spawned on first call for `(project_dir, Rust)` and reused
/// across subsequent calls.
pub(crate) fn rust_analyzer_organize_imports(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<FileEdit>> {
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let end_position = byte_to_lsp_position(&source, source.len());
    let code_action_params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: end_position,
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let did_open_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let response = manager.with_session(project_dir, Language::Rust, |mut client| {
        client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
            &lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: did_open_uri.clone(),
                    language_id: "rust".to_string(),
                    version: 0,
                    text: source.clone(),
                },
            },
        )?;
        client.wait_for_diagnostics(did_open_uri.as_str(), std::time::Duration::from_secs(60));
        let id = client.send_request::<CodeActionRequest>(&code_action_params)?;
        client.read_response::<CodeActionRequest>(id)
    })?;

    let mut all_edits = Vec::new();
    if let Some(actions) = response {
        for action in actions {
            if let lsp_types::CodeActionOrCommand::CodeAction(ca) = action {
                let kind = ca
                    .kind
                    .clone()
                    .unwrap_or_else(|| lsp_types::CodeActionKind::from(""));
                if kind != CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                    && !ca.title.to_ascii_lowercase().contains("organize")
                {
                    continue;
                }
                if let Some(edit) = ca.edit {
                    all_edits.extend(workspace_edit_to_file_edits(edit)?);
                }
            }
        }
    }
    Ok(all_edits)
}

pub(crate) fn existing_target_impl_insert_byte(
    target_path: &Path,
    target_source: &str,
    impl_name: &str,
    router_name: Option<&str>,
) -> Result<Option<TargetImplInsertion>> {
    if target_source.trim().is_empty() {
        return Ok(None);
    }
    let tree = parse_source("rust", target_source)
        .with_context(|| format!("parsing target {}", target_path.display()))?;
    let parsed = ParsedSource {
        path: target_path.to_path_buf(),
        language: "rust",
        source: target_source.to_string(),
        tree,
    };
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for impl_node in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "impl_item")
    {
        let Some(name) = item_name(impl_node, &parsed.source, parsed.language) else {
            continue;
        };
        if name != impl_name {
            continue;
        }
        if !router_matches(&parsed, impl_node, router_name) {
            continue;
        }
        let Some(body) = impl_declaration_list(impl_node) else {
            continue;
        };
        let close = parsed
            .source
            .get(body.start_byte()..body.end_byte())
            .and_then(|text| text.rfind('}').map(|offset| body.start_byte() + offset))
            .ok_or_else(|| anyhow!("matching target impl has no closing brace"))?;
        return Ok(Some(TargetImplInsertion {
            byte: close,
            body_is_empty: parsed.source[body.start_byte() + 1..close]
                .trim()
                .is_empty(),
        }));
    }
    Ok(None)
}

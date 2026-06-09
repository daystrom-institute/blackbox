use super::*;
use crate::lsp::convert::{byte_to_lsp_position, workspace_edit_to_file_edits};

pub(crate) fn plan_add_java_implements(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("add_java_implements only supports java files");
    }

    let interface_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name is required for add_java_implements (fully-qualified or simple interface name)"))?;

    let class_node = if let Some(class_name) = p.impl_name.as_deref() {
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

    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .unwrap_or("(unnamed)")
        .to_string();

    let implements_pos = find_implements_position(class_node);
    let open_brace_pos = find_open_brace_position(class_node, &parsed.source);

    let (insert_pos, insert_text) = if let Some(pos) = implements_pos {
        let existing_impl = &parsed.source[pos..open_brace_pos];
        let trimmed = existing_impl.trim();
        if trimmed.contains('{') {
            let brace_relative = existing_impl.find('{').unwrap_or(0);
            (pos + brace_relative, format!(", {}", interface_name))
        } else {
            (pos, format!(", {}", interface_name))
        }
    } else {
        let name_node = class_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("class has no name node"))?;
        let after_name = name_node.end_byte();
        (after_name, format!(" implements {}", interface_name))
    };

    let edit = TextEdit {
        byte_start: insert_pos,
        byte_end: insert_pos,
        replacement: insert_text,
    };

    let plan = RefactorPlan {
        title: format!("Add implements {} to class {}", interface_name, class_name),
        kind: "add_java_implements".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
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
        operator_opt_outs_used: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_extract_java_interface(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| {
            anyhow!(
                "target is required for extract_java_interface (path for the new interface file)"
            )
        })
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_interface only supports java files");
    }

    let class_node = if let Some(class_name) = p.impl_name.as_deref() {
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

    let class_name = class_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
        .unwrap_or("(unnamed)")
        .to_string();

    let interface_name = p
        .module_name
        .as_deref()
        .unwrap_or_else(|| {
            if class_name.starts_with("Default") && class_name.len() > 7 {
                &class_name[7..]
            } else {
                &class_name
            }
        })
        .to_string();

    // Gap 14: use java_default_target_prelude so the interface file
    // gets BOTH a package decl AND every import from the source. Tree-
    // sitter / javac don't error on unused imports (just unused-import
    // warnings), and copying the full set means every signature type
    // (project types, JDK, third-party) resolves out of the box. A
    // follow-up `java_lsp_organize_imports` can prune later if the
    // operator cares about minimal imports.
    let target_pkg = resolve_java_target_package(p, &parsed.source, &source_path, &target_path)
        .ok()
        .flatten();
    let prelude = java_default_target_prelude(p, &parsed.source, target_pkg.as_deref());

    let methods = java_methods(&parsed);
    let class_methods: Vec<&JavaMethod> = methods
        .iter()
        .filter(|m| m.parent_name == class_name)
        .collect();

    let selected_names: HashSet<&str> = if let Some(names) = p.item_names.as_deref() {
        names.iter().map(String::as_str).collect()
    } else {
        class_methods
            .iter()
            .filter(|m| {
                let method_node = parsed.tree.root_node();
                let node = find_node(method_node, |n: Node<'_>| {
                    n.kind() == "method_declaration"
                        && n.start_byte() == m.item.byte_start
                        && n.end_byte() == m.item.byte_end
                });
                node.is_some()
                    && node.unwrap().kind() == "method_declaration"
                    && method_is_public(node.unwrap())
                    && !method_is_static(node.unwrap())
                    && m.item.name.as_deref() != Some("<init>")
            })
            .filter_map(|m| m.item.name.as_deref())
            .collect()
    };

    if selected_names.is_empty() {
        bail!(
            "no public non-static methods found to extract; pass item_names to select specific methods"
        );
    }

    let mut interface_sigs = Vec::new();
    let mut methods_needing_visibility_widen = Vec::new();
    let mut class_type_params: HashSet<String> = HashSet::new();
    if let Some(tp_text) = class_type_parameters_text(class_node, &parsed.source) {
        for chunk in tp_text
            .trim_start_matches('<')
            .trim_end_matches('>')
            .split(',')
        {
            let ident = chunk.split_whitespace().last().unwrap_or("").trim();
            if !ident.is_empty() {
                class_type_params.insert(ident.to_string());
            }
        }
    }

    let mut used_type_params: HashSet<String> = HashSet::new();

    for method in &class_methods {
        let name = match method.item.name.as_deref() {
            Some(n) => n,
            None => continue,
        };
        if !selected_names.contains(name) {
            continue;
        }

        let method_node = find_node(parsed.tree.root_node(), |n: Node<'_>| {
            n.kind() == "method_declaration"
                && n.start_byte() == method.item.byte_start
                && n.end_byte() == method.item.byte_end
        });
        let method_node = match method_node {
            Some(n) => n,
            None => continue,
        };

        if name == class_name {
            continue;
        }

        let is_pub = method_is_public(method_node);
        if !is_pub {
            methods_needing_visibility_widen.push(method.item.byte_start);
        }

        let sig = extract_interface_method_signature(method_node, &parsed.source);
        interface_sigs.push(sig);

        let sig_type_params = collect_type_params_in_signature(method_node, &parsed.source);
        for tp in &sig_type_params {
            if class_type_params.contains(tp.as_str()) {
                used_type_params.insert(tp.clone());
            }
        }
    }

    if interface_sigs.is_empty() {
        bail!("no valid method signatures could be extracted");
    }

    let interface_type_params = if !used_type_params.is_empty() {
        let mut ordered = Vec::new();
        if let Some(tp_text) = class_type_parameters_text(class_node, &parsed.source) {
            let inner = tp_text.trim_start_matches('<').trim_end_matches('>');
            for chunk in inner.split(',') {
                let ident = chunk.split_whitespace().last().unwrap_or("").trim();
                if used_type_params.contains(ident) {
                    ordered.push(chunk.trim().to_string());
                }
            }
        }
        if ordered.is_empty() {
            String::new()
        } else {
            format!("<{}>", ordered.join(", "))
        }
    } else {
        String::new()
    };

    let mut interface_content = String::new();
    interface_content.push_str(&prelude);
    interface_content.push_str(&format!(
        "public interface {}{} {{\n",
        interface_name, interface_type_params
    ));
    for sig in &interface_sigs {
        for line in sig.lines() {
            interface_content.push_str(&format!("    {}\n", line));
        }
        interface_content.push('\n');
    }
    if interface_content.ends_with("\n\n") {
        interface_content.pop();
    }
    interface_content.push_str("}\n");

    let mut edits = Vec::new();

    edits.push(FileEdit {
        path: path_string(&target_path),
        original_sha256: sha256_hex(&[]),
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: interface_content,
        }],
        new_text: None,
    });

    let implements_pos = find_implements_position(class_node);
    let open_brace_pos = find_open_brace_position(class_node, &parsed.source);

    let (impl_insert_pos, impl_insert_text) = if let Some(pos) = implements_pos {
        let existing_impl = &parsed.source[pos..open_brace_pos];
        if existing_impl.contains('{') {
            let brace_relative = existing_impl.find('{').unwrap_or(0);
            (pos + brace_relative, format!(", {}", interface_name))
        } else {
            (pos, format!(", {}", interface_name))
        }
    } else {
        let name_node = class_node
            .child_by_field_name("name")
            .ok_or_else(|| anyhow!("class has no name node"))?;
        let after_name = name_node.end_byte();
        (after_name, format!(" implements {}", interface_name))
    };

    let mut source_edits = vec![TextEdit {
        byte_start: impl_insert_pos,
        byte_end: impl_insert_pos,
        replacement: impl_insert_text,
    }];

    for method_start in &methods_needing_visibility_widen {
        let method_node = find_node(parsed.tree.root_node(), |n: Node<'_>| {
            n.kind() == "method_declaration" && n.start_byte() == *method_start
        });
        if let Some(mn) = method_node {
            let mods = collect_java_modifiers(mn);
            let edit = build_visibility_rewrite_edit(mn, &mods, Some("public"), &parsed.source);
            source_edits.push(edit);
        }
    }

    source_edits.sort_by_key(|e| e.byte_start);

    edits.push(FileEdit {
        path: path_string(&source_path),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
        new_text: None,
    });

    let validations = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!(
            "Extract interface {} from class {} ({} methods), add implements clause",
            interface_name,
            class_name,
            interface_sigs.len()
        ),
        kind: "extract_java_interface".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
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
        operator_opt_outs_used: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

pub(crate) fn plan_migrate_java_type_usages(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("migrate_java_type_usages only supports java files");
    }

    let class_name = p.module_name.as_deref().ok_or_else(|| {
        anyhow!(
            "module_name is required for migrate_java_type_usages (simple class name to replace)"
        )
    })?;

    let new_name = p.new_text.as_deref().ok_or_else(|| {
        anyhow!("new_text is required for migrate_java_type_usages (replacement type name)")
    })?;

    let _source_class = extract_java_package(&parsed.source)
        .map(|pkg| format!("{}.{}", pkg, class_name))
        .unwrap_or_else(|| class_name.to_string());

    let mut edits = Vec::new();
    let positions = find_type_use_positions_in_file(&parsed.source, &parsed.tree, class_name);

    for (start, end) in positions {
        let context_start = start.saturating_sub(40);
        let context_end = (end + 40).min(parsed.source.len());
        let after = &parsed.source[end..context_end];
        let trimmed_after = after.trim_start();
        if trimmed_after.starts_with('.') || trimmed_after.starts_with('(') {
            continue;
        }
        let before = &parsed.source[context_start..start];
        if before.ends_with("new ") || before.ends_with("new\t") {
            continue;
        }

        edits.push(TextEdit {
            byte_start: start,
            byte_end: end,
            replacement: new_name.to_string(),
        });
    }

    if edits.is_empty() {
        bail!(
            "no type-use positions found for `{}` in {}",
            class_name,
            source_path.display()
        );
    }

    edits.sort_by_key(|e| e.byte_start);
    let n = edits.len();

    let plan = RefactorPlan {
        title: format!(
            "Migrate {} type-use(s) of {} to {} in {}",
            n,
            class_name,
            new_name,
            source_path.display()
        ),
        kind: "migrate_java_type_usages".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
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
        operator_opt_outs_used: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Ask JDTLS for `source.organizeImports` code actions on
/// `source_path` using the shared session pool. The session is
/// lazily spawned on first call for `(project_dir, Java)` and reused
/// across subsequent calls.
pub(crate) fn jdtls_organize_imports(
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
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
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

    let response = manager.with_session(project_dir, Language::Java, |mut client| {
        // JDTLS indexes lazily per-file: even after the post-`initialized`
        // workspace-ready drain, the very first source.organizeImports
        // request against an unopened file returns degenerate edits (no
        // classpath-aware unused-import pruning, and on some JDTLS
        // versions an edit whose replacement drops the trailing newline
        // before the type declaration). Open the file and wait for the
        // first publishDiagnostics so JDTLS has actually loaded and
        // type-checked it before we ask for the organize edit.
        client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
            &lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: source_uri.clone(),
                    language_id: "java".to_string(),
                    version: 0,
                    text: source.clone(),
                },
            },
        )?;
        client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
        let id = client.send_request::<CodeActionRequest>(&code_action_params)?;
        client
            .read_response::<CodeActionRequest>(id)
            .map_err(|e| match e {
                LspError::Broken(b) => LspError::Broken(b),
                LspError::Other(o) => LspError::Other(o),
            })
    })?;

    let mut all_edits = Vec::new();
    if let Some(actions) = response {
        for action in actions {
            if let lsp_types::CodeActionOrCommand::CodeAction(ca) = action {
                let kind = ca
                    .kind
                    .clone()
                    .unwrap_or_else(|| lsp_types::CodeActionKind::from(""));
                if kind == CodeActionKind::SOURCE_ORGANIZE_IMPORTS
                    || ca.title.to_ascii_lowercase().contains("organize")
                {
                    if let Some(edit) = ca.edit {
                        all_edits.extend(workspace_edit_to_file_edits(edit)?);
                    }
                }
            }
        }
    }
    Ok(all_edits)
}

pub(crate) fn plan_java_lsp_organize_imports(
    p: &RefactorPlanParams,
    ctx: &PlanContext,
) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let project_dir_str = project_dir.to_string_lossy();
    let source_path = resolve_path(Some(&project_dir_str), &p.source)?;

    let lsp_attempt = ctx
        .lsp
        .as_ref()
        .map(|mgr| jdtls_organize_imports(mgr, &project_dir, &source_path));
    let (file_edits, semantic_status) = match lsp_attempt {
        Some(Ok(edits)) if !edits.is_empty() => (edits, SemanticStatus::LspVerified),
        _ => {
            if let Some(Err(err)) = &lsp_attempt {
                tracing::debug!(error = %err, "JDTLS organize_imports failed; falling back to heuristic");
            }
            (
                heuristic_java_organize_imports(&project_dir, &source_path)?,
                SemanticStatus::SyntaxOnly,
            )
        }
    };
    if file_edits.is_empty() {
        bail!("no Java import organization edits needed");
    }

    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let plan = RefactorPlan {
        title: format!("Organize Java imports in {}", p.source),
        kind: "java_lsp_organize_imports".to_string(),
        semantic_status,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
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
        operator_opt_outs_used: Vec::new(),
    };

    Ok(serde_json::to_string_pretty(&plan)?)
}

//! `extract_rust_items_to_submodule` compound plan kind.
//!
//! One plan that does what `extract_rust_items` + `add_rust_mod_decl` +
//! `add_rust_use_decl` + `rewrite_rust_item_visibility` +
//! `rewrite_rust_field_visibility` had to do as five separate
//! roundtrips when splitting a monster module. Inputs:
//!
//! - `source`: parent module file.
//! - `target`: new submodule file (must be empty or missing).
//! - `item_names` + optional `item_kinds`: items to move.
//! - `module_name`: defaults to the target file stem. Used for the
//!   `mod <module_name>;` declaration on the parent.
//! - `visibility`: visibility floor applied to moved items AND their
//!   struct fields (when moving a struct). Defaults to `pub(super)`.
//! - `target_prelude`: text inserted at the top of the new file before
//!   the moved items. Defaults to `use super::*;`.
//!
//! Behavior:
//! - Source FileEdit: `mod <module_name>;` insertion, `use
//!   <module_name>::{...};` insertion, deletion of each moved item's
//!   leading-trivia-to-trailing-trivia span. Edits are merged into one
//!   FileEdit sorted by byte offset.
//! - Target FileEdit: writes the prelude + the moved item texts with
//!   item-level visibility AND struct-field visibility already bumped
//!   to `visibility`. The visibility transforms are baked into the
//!   target text, not emitted as separate edits — so the plan applies
//!   in one pass with no edit ordering dependencies.
//!
//! Refuses to overwrite a non-empty target.

use super::*;

pub(crate) fn plan_extract_rust_items_to_submodule(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_items_to_submodule"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let visibility = p.visibility.as_deref().unwrap_or("pub(super)");
    let visibility_prefix = rust_decl_visibility_prefix(Some(visibility))?;
    let module_name: String = match p.module_name.as_deref() {
        Some(name) => name.to_string(),
        None => target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "module_name not given and target {} has no file stem to derive from",
                    target_path.display()
                )
            })?
            .to_string(),
    };
    validate_rust_identifier(&module_name, "module_name")?;
    let target_prelude = p.target_prelude.as_deref().unwrap_or("use super::*;");

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = select_top_level_items_local(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;

    // Render each moved item with item-level visibility + (struct) field
    // visibility bumped. Bake into target text so no sequencing edits run.
    let mut moved_blocks: Vec<String> = Vec::new();
    for item in &selected {
        moved_blocks.push(render_item_with_visibility_bumped(&parsed, item, visibility_prefix)?);
    }

    // Source-side: mod_decl + use_decl insertions (skip if already present).
    let mod_decl_edit =
        compute_mod_decl_edit_idempotent(&parsed.source, &items, &module_name)?;
    let use_path_str = build_use_path(&module_name, &selected);
    let use_decl_edit =
        compute_use_decl_edit_idempotent(&parsed.source, &items, &use_path_str)?;

    // Source-side deletions for each moved item.
    let mut source_edits: Vec<TextEdit> = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect();
    if let Some(e) = mod_decl_edit {
        source_edits.push(e);
    }
    if let Some(e) = use_decl_edit {
        source_edits.push(e);
    }
    source_edits.sort_by_key(|e| e.byte_start);
    ensure_non_overlapping(&source_edits)
        .context("extract_rust_items_to_submodule source edits overlap")?;

    // Target-side: refuse non-empty existing target, then write the
    // prelude + visibility-bumped moved blocks.
    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    if !target_source.trim().is_empty() {
        bail!(
            "target file {} already exists and is non-empty; this plan kind \
             refuses to overwrite",
            target_path.display()
        );
    }
    let mut target_body = String::new();
    if !target_prelude.is_empty() {
        target_body.push_str(target_prelude.trim_end());
        target_body.push_str("\n\n");
    }
    for (i, block) in moved_blocks.iter().enumerate() {
        if i > 0 {
            target_body.push_str("\n\n");
        }
        let trimmed = block.trim_start_matches('\n').trim_end_matches('\n');
        target_body.push_str(trimmed);
    }
    if !target_body.ends_with('\n') {
        target_body.push('\n');
    }
    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: target_source.len(),
        replacement: target_body,
    };

    let plan = RefactorPlan {
        title: format!(
            "extract {} Rust item(s) from {} into submodule `{}` at {}",
            selected.len(),
            path_string(&source_path),
            module_name,
            path_string(&target_path)
        ),
        kind: "extract_rust_items_to_submodule".to_string(),
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
                edits: vec![target_edit],
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

    validate_plan_shape(&plan)
        .context("validate extract_rust_items_to_submodule plan")?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Local select_items that only handles top-level items. The compound
/// primitive doesn't support impl_method moves — use
/// `extract_rust_impl_methods` for that case.
fn select_top_level_items_local<'a>(
    items: &'a [SyntaxItem],
    names: Option<&[String]>,
    kinds: Option<&[String]>,
) -> Result<Vec<&'a SyntaxItem>> {
    let names = names
        .filter(|xs| !xs.is_empty())
        .ok_or_else(|| anyhow!("item_names is required"))?;
    if let Some(ks) = kinds {
        if ks.iter().any(|k| k == "impl_method") {
            bail!(
                "extract_rust_items_to_submodule does not support impl_method; \
                 use extract_rust_impl_methods + the parent-rewiring primitives \
                 separately"
            );
        }
    }
    let kind_set = kinds
        .map(|xs| xs.iter().map(String::as_str).collect::<HashSet<_>>());
    let mut selected: Vec<&SyntaxItem> = Vec::new();
    for expected in names {
        let mut matches = items
            .iter()
            .filter(|i| i.name.as_deref() == Some(expected.as_str()))
            .filter(|i| match &kind_set {
                Some(set) => set.contains(i.kind.as_str()),
                None => true,
            })
            .collect::<Vec<_>>();
        match matches.len() {
            0 => bail!("requested item `{expected}` was not found"),
            1 => selected.push(matches.remove(0)),
            _ => bail!(
                "requested item `{expected}` matched multiple declarations; narrow with item_kinds"
            ),
        }
    }
    Ok(selected)
}

/// Render an item's full text (leading_trivia_start..byte_end) with
/// item-level visibility rewritten to `visibility_prefix` and, for
/// struct items, every named field's visibility likewise bumped.
fn render_item_with_visibility_bumped(
    parsed: &ParsedSource,
    item: &SyntaxItem,
    visibility_prefix: &str,
) -> Result<String> {
    let base = item.leading_trivia_start;
    let original = parsed
        .source
        .get(base..item.byte_end)
        .ok_or_else(|| anyhow!("invalid item range for {}", item.plan_local_id))?
        .to_string();

    // Collect (absolute_start, absolute_end, replacement) for every
    // visibility rewrite needed inside the item.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    // Item-level visibility.
    let keyword = rust_visibility_keyword_byte(&parsed.source, item)?;
    let vis_start = rust_item_visibility_start_byte(&parsed.source, item, keyword);
    let current_prefix = &parsed.source[vis_start..keyword];
    let qualifier_prefix = rust_strip_visibility_prefix(current_prefix);
    let new_prefix = format!("{visibility_prefix}{qualifier_prefix}");
    if current_prefix != new_prefix {
        edits.push((vis_start, keyword, new_prefix));
    }

    // Struct fields.
    if item.kind == "struct_item" {
        if let Ok(fields) = rust_named_struct_fields(parsed, item) {
            for field in fields {
                let f_vis_start = rust_item_visibility_start_byte(
                    &parsed.source,
                    &field.item,
                    field.name_byte_start,
                );
                let f_current = &parsed.source[f_vis_start..field.name_byte_start];
                if f_current != visibility_prefix {
                    edits.push((
                        f_vis_start,
                        field.name_byte_start,
                        visibility_prefix.to_string(),
                    ));
                }
            }
        }
    }

    // Apply edits to a cloned buffer in reverse order (preserves earlier
    // offsets). Skip edits whose absolute range falls outside the item
    // (shouldn't happen, defensive).
    edits.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = original;
    for (start, end, repl) in edits {
        if start < base || end < base {
            continue;
        }
        let rel_start = start - base;
        let rel_end = end - base;
        if rel_end > out.len() {
            continue;
        }
        out.replace_range(rel_start..rel_end, &repl);
    }
    Ok(out)
}

/// Idempotent `mod <name>;` insertion. Returns None when the module is
/// already declared in the source file.
fn compute_mod_decl_edit_idempotent(
    source: &str,
    items: &[SyntaxItem],
    module_name: &str,
) -> Result<Option<TextEdit>> {
    if items
        .iter()
        .any(|i| i.kind == "mod_item" && i.name.as_deref() == Some(module_name))
    {
        return Ok(None);
    }
    let declaration = format!("mod {module_name};");
    let last_mod = items
        .iter()
        .filter(|item| item.kind == "mod_item")
        .filter(|item| ensure_rust_mod_declaration(source, item).is_ok())
        .max_by_key(|item| item.byte_end);
    let (insert_at, replacement) = if let Some(item) = last_mod {
        (item.byte_end, format!("\n{declaration}"))
    } else {
        (
            rust_module_decl_fallback_insert_byte(source),
            format!("{declaration}\n"),
        )
    };
    Ok(Some(TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement,
    }))
}

/// Idempotent `use <use_path>;` insertion. Returns None when the same
/// declaration already exists verbatim.
fn compute_use_decl_edit_idempotent(
    source: &str,
    items: &[SyntaxItem],
    use_path: &str,
) -> Result<Option<TextEdit>> {
    let declaration = format!("use {use_path};");
    if source.lines().any(|line| line.trim() == declaration) {
        return Ok(None);
    }
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
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(source));
    let replacement = if source[insert_at..].starts_with('\n') {
        format!("\n{declaration}")
    } else if insert_at == source.len() || source[..insert_at].ends_with('\n') {
        format!("{declaration}\n")
    } else {
        format!("\n{declaration}\n")
    };
    Ok(Some(TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement,
    }))
}

/// Build a `<module>::{Name1, fn2, ...}` use path from selected items.
/// Single-item form is `<module>::Name`. Multi-item form lists every
/// moved item alphabetically inside `{}`.
fn build_use_path(module_name: &str, selected: &[&SyntaxItem]) -> String {
    let mut names: Vec<&str> = selected
        .iter()
        .filter_map(|i| i.name.as_deref())
        .collect();
    names.sort();
    match names.as_slice() {
        [] => module_name.to_string(),
        [single] => format!("{module_name}::{single}"),
        many => format!("{module_name}::{{{}}}", many.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(
        source: &Path,
        target: &Path,
        item_names: &[&str],
        item_kinds: Option<Vec<&str>>,
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "extract_rust_items_to_submodule".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            item_names: Some(item_names.iter().map(|s| s.to_string()).collect()),
            item_kinds: item_kinds.map(|ks| ks.iter().map(|s| s.to_string()).collect()),
            impl_name: None,
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
            boolean_getter_strategy: None,
            callback_externals: None,
            output_path: None,
        }
    }

    fn apply_file_edit(path: &Path, fe: &FileEdit) -> String {
        let original = if path.exists() {
            fs::read_to_string(path).unwrap()
        } else {
            String::new()
        };
        let mut sorted = fe.edits.clone();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        let mut out = original;
        for edit in &sorted {
            out.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        out
    }

    // Gate: end-to-end — struct + free fn moved, mod_decl + use_decl
    // injected, visibility bumped on item AND fields.
    #[test]
    fn moves_struct_and_fn_with_visibility_bump_and_wiring() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(
            &src,
            "pub fn outer() {}\n\nstruct Hidden {\n    name: String,\n    kind: u32,\n}\n\nfn helper() -> usize { 42 }\n",
        )
        .unwrap();

        let plan_json = plan_extract_rust_items_to_submodule(&make_params(
            &src,
            &tgt,
            &["Hidden", "helper"],
            Some(vec!["struct_item", "function_item"]),
        ))
        .expect("plan should succeed");
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(plan.edits.len(), 2);

        let source_after = apply_file_edit(&src, &plan.edits[0]);
        let target_after = apply_file_edit(&tgt, &plan.edits[1]);

        // Source: mod decl + use decl present, moved items gone.
        assert!(
            source_after.contains("mod child;"),
            "mod_decl missing: {source_after}"
        );
        assert!(
            source_after.contains("use child::{Hidden, helper};")
                || source_after.contains("use child::{helper, Hidden};"),
            "use_decl missing: {source_after}"
        );
        assert!(
            !source_after.contains("struct Hidden"),
            "Hidden struct should be removed from source: {source_after}"
        );
        assert!(
            !source_after.contains("fn helper"),
            "helper fn should be removed from source: {source_after}"
        );

        // Target: prelude + visibility-bumped items.
        assert!(
            target_after.contains("use super::*;"),
            "target prelude missing: {target_after}"
        );
        assert!(
            target_after.contains("pub(super) struct Hidden"),
            "struct visibility not bumped: {target_after}"
        );
        assert!(
            target_after.contains("pub(super) name: String"),
            "field visibility not bumped: {target_after}"
        );
        assert!(
            target_after.contains("pub(super) kind: u32"),
            "field visibility not bumped: {target_after}"
        );
        assert!(
            target_after.contains("pub(super) fn helper"),
            "fn visibility not bumped: {target_after}"
        );
    }

    // Gate: explicit visibility (`pub(crate)`) is honored on items + fields.
    #[test]
    fn honors_explicit_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(
            &src,
            "struct Data { value: u32 }\nfn doit() {}\n",
        )
        .unwrap();

        let mut params = make_params(&src, &tgt, &["Data", "doit"], None);
        params.visibility = Some("pub(crate)".to_string());

        let plan_json = plan_extract_rust_items_to_submodule(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let target_after = apply_file_edit(&tgt, &plan.edits[1]);
        assert!(
            target_after.contains("pub(crate) struct Data"),
            "struct visibility: {target_after}"
        );
        assert!(
            target_after.contains("pub(crate) value: u32"),
            "field visibility: {target_after}"
        );
        assert!(
            target_after.contains("pub(crate) fn doit"),
            "fn visibility: {target_after}"
        );
    }

    // Gate: target_prelude can be overridden (e.g. for items that don't
    // need to see the parent's `use super::*;`).
    #[test]
    fn honors_custom_target_prelude() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(&src, "fn x() {}\n").unwrap();
        let mut params = make_params(&src, &tgt, &["x"], Some(vec!["function_item"]));
        params.target_prelude = Some("use std::collections::HashMap;".to_string());

        let plan_json = plan_extract_rust_items_to_submodule(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let target_after = apply_file_edit(&tgt, &plan.edits[1]);
        assert!(
            target_after.contains("use std::collections::HashMap;"),
            "custom prelude: {target_after}"
        );
        assert!(
            !target_after.contains("use super::*;"),
            "default prelude must NOT appear when overridden: {target_after}"
        );
    }

    // Gate: target file stem derives module_name when not given.
    #[test]
    fn derives_module_name_from_target_stem() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/cross_file.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(&src, "fn x() {}\n").unwrap();
        let plan_json = plan_extract_rust_items_to_submodule(&make_params(
            &src,
            &tgt,
            &["x"],
            Some(vec!["function_item"]),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let source_after = apply_file_edit(&src, &plan.edits[0]);
        assert!(
            source_after.contains("mod cross_file;"),
            "module name should derive from target stem: {source_after}"
        );
        assert!(
            source_after.contains("use cross_file::x;"),
            "use decl should use derived name: {source_after}"
        );
    }

    // Gate: explicit module_name overrides target-stem derivation.
    #[test]
    fn explicit_module_name_overrides_stem() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/internal.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(&src, "fn x() {}\n").unwrap();
        let mut params = make_params(&src, &tgt, &["x"], Some(vec!["function_item"]));
        params.module_name = Some("private_helpers".to_string());
        let plan_json = plan_extract_rust_items_to_submodule(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let source_after = apply_file_edit(&src, &plan.edits[0]);
        // Note: target stem is "internal" but module_name forces "private_helpers".
        // This case would create a mismatch (mod private_helpers; points to ?), but
        // the plan kind doesn't enforce target-stem == module_name. Operator's
        // responsibility to keep them aligned.
        assert!(
            source_after.contains("mod private_helpers;"),
            "explicit module name should override: {source_after}"
        );
    }

    // Gate: refuse to overwrite a non-empty existing target.
    #[test]
    fn refuses_non_empty_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(&src, "fn x() {}\n").unwrap();
        fs::write(&tgt, "// pre-existing\nfn keep() {}\n").unwrap();
        let err = plan_extract_rust_items_to_submodule(&make_params(
            &src,
            &tgt,
            &["x"],
            Some(vec!["function_item"]),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("already exists and is non-empty"),
            "expected non-empty refusal, got: {err}"
        );
    }

    // Gate: refuse impl_method kind (caller should use extract_rust_impl_methods).
    #[test]
    fn refuses_impl_method() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(&src, "fn x() {}\n").unwrap();
        let err = plan_extract_rust_items_to_submodule(&make_params(
            &src,
            &tgt,
            &["x"],
            Some(vec!["impl_method"]),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("does not support impl_method"),
            "expected impl_method refusal, got: {err}"
        );
    }

    // Gate: pre-existing mod_decl is not duplicated; pre-existing use_decl
    // identical to the one we'd insert is not duplicated either.
    #[test]
    fn idempotent_mod_and_use_decls() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("parent.rs");
        let tgt = dir.path().join("parent/child.rs");
        fs::create_dir_all(tgt.parent().unwrap()).unwrap();
        fs::write(
            &src,
            "mod child;\nuse child::x;\n\nfn x() {}\n",
        )
        .unwrap();
        let plan_json = plan_extract_rust_items_to_submodule(&make_params(
            &src,
            &tgt,
            &["x"],
            Some(vec!["function_item"]),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let source_after = apply_file_edit(&src, &plan.edits[0]);
        // Only one mod_decl + one use_decl in the result.
        let mod_count = source_after.matches("mod child;").count();
        let use_count = source_after.matches("use child::x;").count();
        assert_eq!(mod_count, 1, "mod_decl duplicated: {source_after}");
        assert_eq!(use_count, 1, "use_decl duplicated: {source_after}");
    }
}

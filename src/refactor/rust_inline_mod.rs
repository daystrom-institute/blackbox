//! `inline_mod_to_file_submodule` plan kind.
//!
//! Extracts the body of an inline `mod foo { ... }` block into a submodule
//! file and replaces the block with a `mod foo;` declaration pointing at
//! the new file. Outer attributes such as `#[cfg(test)]` stay attached to
//! the retained declaration — they're written above `mod foo`, not inside
//! the body, so the in-place rewrite naturally preserves them.
//!
//! Target path is auto-derived from the source path when not given:
//!   `src/refactor/java.rs` + `mod tests` → `src/refactor/java/tests.rs`
//!   `src/refactor/mod.rs`  + `mod tests` → `src/refactor/tests.rs`
//!   `src/lib.rs`           + `mod foo`   → `src/foo.rs`
//!
//! Explicit `target` overrides the derivation.

use super::*;
use regex::Regex;
use std::path::PathBuf;

pub(crate) fn plan_inline_mod_to_file_submodule(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;

    let item_names = p
        .item_names
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for inline_mod_to_file_submodule"))?;
    if item_names.len() != 1 {
        bail!(
            "inline_mod_to_file_submodule moves exactly one mod at a time; got {} names",
            item_names.len()
        );
    }
    let mod_name = &item_names[0];

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let item = items
        .iter()
        .find(|i| i.kind == "mod_item" && i.name.as_deref() == Some(mod_name.as_str()))
        .ok_or_else(|| {
            anyhow!(
                "mod_item `{mod_name}` not found in {}",
                source_path.display()
            )
        })?
        .clone();

    // Locate the AST node and reject already-file submodule declarations
    // (`mod foo;` with no body block).
    let root = parsed.tree.root_node();
    let mod_node = rust_node_by_range(root, "mod_item", item.byte_start, item.byte_end)
        .ok_or_else(|| anyhow!("could not locate mod_item AST node for `{mod_name}`"))?;
    let body_node = mod_node.child_by_field_name("body").ok_or_else(|| {
        anyhow!(
            "mod `{mod_name}` is already a file submodule declaration \
             (`mod {mod_name};`); nothing to extract"
        )
    })?;
    if body_node.kind() != "declaration_list" {
        bail!(
            "expected mod_item body to be `declaration_list`, got `{}`",
            body_node.kind()
        );
    }

    // Body content lives between `{` (body_node.start_byte()) and `}`
    // (body_node.end_byte() - 1, exclusive of the closing brace).
    let block_start = body_node.start_byte();
    let block_end = body_node.end_byte();
    let body_text = parsed
        .source
        .get(block_start + 1..block_end - 1)
        .ok_or_else(|| anyhow!("invalid body range for mod `{mod_name}`"))?;
    let de_indented = de_indent_body(body_text);

    // Decide target path early so we can compute depth for fixture repair.
    let target_path = match p.target.as_deref() {
        Some(t) => resolve_path(p.project_dir.as_deref(), t)?,
        None => derive_submodule_target_path(&source_path, mod_name)?,
    };
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    // When extracting a #[cfg(test)] module to a deeper path, repair
    // relative fixture paths in include_str!/include_bytes!/include!.
    let de_indented = {
        let is_test_mod = has_cfg_test_attr(mod_node, &parsed.source);
        let extra_depth = compute_depth_increase(&source_path, &target_path);
        if is_test_mod && extra_depth > 0 {
            repair_include_paths(&de_indented, extra_depth)?
        } else {
            de_indented
        }
    };

    let target_content = if de_indented.ends_with('\n') {
        de_indented
    } else {
        format!("{de_indented}\n")
    };

    // Refuse to overwrite a non-empty file.
    let target_source = if target_path.exists() {
        let existing = fs::read_to_string(&target_path)
            .with_context(|| format!("failed to read {}", target_path.display()))?;
        if !existing.trim().is_empty() {
            bail!(
                "target file {} already exists and is non-empty",
                target_path.display()
            );
        }
        existing
    } else {
        String::new()
    };

    // Source edit: replace `{ ... }` (plus any whitespace immediately before
    // `{`) with `;`. This preserves the `mod NAME` prefix and any outer
    // attributes / leading doc comments untouched.
    let prefix_ws_start = trim_trailing_ws_end(parsed.source.as_bytes(), block_start);
    let source_edit = TextEdit {
        byte_start: prefix_ws_start,
        byte_end: block_end,
        replacement: ";".to_string(),
    };

    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: target_source.len(),
        replacement: target_content,
    };

    let plan = RefactorPlan {
        title: format!(
            "extract inline `mod {mod_name}` from {} into submodule file {}",
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "inline_mod_to_file_submodule".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: vec![source_edit],
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
        items: vec![item],
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

    validate_plan_shape(&plan).context("validate inline_mod_to_file_submodule plan")?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Derive the submodule file path for `mod_name` based on the parent
/// source path. Rust 2018+ module layout:
/// - `parent/mod.rs` or `lib.rs` or `main.rs` → submodule lives in the
///   same directory: `<dir>/<mod_name>.rs`.
/// - Any other `<stem>.rs` → submodule lives in `<dir>/<stem>/<mod_name>.rs`.
fn derive_submodule_target_path(source_path: &Path, mod_name: &str) -> Result<PathBuf> {
    let parent = source_path
        .parent()
        .ok_or_else(|| anyhow!("source path has no parent: {}", source_path.display()))?;
    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("source path has no file stem: {}", source_path.display()))?;
    let target = if matches!(stem, "mod" | "lib" | "main") {
        parent.join(format!("{mod_name}.rs"))
    } else {
        parent.join(stem).join(format!("{mod_name}.rs"))
    };
    Ok(target)
}

/// Strip a single leading newline (if any) and the longest common run of
/// leading spaces across all non-blank lines. Tabs are not collapsed —
/// operators using tab indentation get the body verbatim and can
/// `cargo fmt` after.
fn de_indent_body(body: &str) -> String {
    let body = body.strip_prefix('\n').unwrap_or(body);
    let mut common: Option<usize> = None;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let lead = line.bytes().take_while(|b| *b == b' ').count();
        common = Some(match common {
            Some(c) => c.min(lead),
            None => lead,
        });
    }
    let strip = common.unwrap_or(0);
    if strip == 0 {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    let mut first = true;
    for line in body.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        if line.trim().is_empty() {
            out.push_str(line);
        } else if line.len() >= strip {
            out.push_str(&line[strip..]);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Walk backward from `end` skipping ` ` and `\t` bytes; return the
/// resulting position. Used to absorb whitespace between `mod NAME` and
/// `{` into the replacement.
fn trim_trailing_ws_end(bytes: &[u8], end: usize) -> usize {
    let mut i = end;
    while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
        i -= 1;
    }
    i
}

fn has_cfg_test_attr(mod_node: Node, source: &str) -> bool {
    let mut node = mod_node;
    loop {
        match node.prev_named_sibling() {
            Some(prev) if prev.kind() == "attribute_item" => {
                let text = &source[prev.start_byte()..prev.end_byte()];
                if is_cfg_test_attr(text) {
                    return true;
                }
                node = prev;
            }
            _ => return false,
        }
    }
}

fn is_cfg_test_attr(text: &str) -> bool {
    let re = Regex::new(r#"cfg\s*\([^)]*\btest\b"#).unwrap();
    re.is_match(text)
}

fn compute_depth_increase(source_path: &Path, target_path: &Path) -> usize {
    let source_dir = match source_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => return 0,
    };
    let target_dir = match target_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => return 0,
    };
    let source_str = source_dir.to_string_lossy();
    let target_str = target_dir.to_string_lossy();
    if target_str == source_str || !target_str.starts_with(source_str.as_ref()) {
        return 0;
    }
    let rest = &target_str[source_str.len()..];
    rest.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .count()
}

fn repair_include_paths(body: &str, extra_depth: usize) -> Result<String> {
    if extra_depth == 0 {
        return Ok(body.to_string());
    }
    let prefix = "../".repeat(extra_depth);
    let simple_re =
        Regex::new(r#"((?:include_str|include_bytes|include)!)\s*\(\s*"([^"]*)"\s*\)"#)?;
    let any_re = Regex::new(r#"(?:include_str|include_bytes|include)!\s*\(\s*[^"]*"#)?;
    let simple_count = simple_re.find_iter(body).count();
    let any_count = any_re.find_iter(body).count();
    if any_count > simple_count {
        bail!(
            "found include_str!/include_bytes!/include! with non-string-literal arguments; \
             cannot safely repair fixture paths when moving module deeper"
        );
    }
    if simple_count == 0 {
        return Ok(body.to_string());
    }
    let result = simple_re.replace_all(body, |caps: &regex::Captures| {
        let path = caps.get(2).unwrap().as_str();
        if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
            return caps.get(0).unwrap().as_str().to_string();
        }
        let new_path = format!("{prefix}{path}");
        let full = caps.get(0).unwrap();
        let path_group = caps.get(2).unwrap();
        let before = &body[full.start()..path_group.start()];
        let after = &body[path_group.end()..full.end()];
        format!("{before}{new_path}{after}")
    });
    Ok(result.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(source: &Path, mod_name: &str, target: Option<&Path>) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "inline_mod_to_file_submodule".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: target.map(|p| p.to_string_lossy().into_owned()),
            item_names: Some(vec![mod_name.to_string()]),
            item_kinds: None,
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
            declaring_class: None,
            summary_only: None,
            propagate_class_annotations: None,
            source_delegate_wrappers: None,
            wiring_mode: None,
        }
    }

    fn apply_to(plan_json: &str, file_idx: usize) -> String {
        let plan: RefactorPlan = serde_json::from_str(plan_json).unwrap();
        let fe = &plan.edits[file_idx];
        let original = if Path::new(&fe.path).exists() {
            fs::read_to_string(&fe.path).unwrap()
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

    // Gate: simple inline mod tests extraction with auto-derived target path.
    #[test]
    fn extracts_inline_mod_tests_to_sibling_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/refactor");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("java.rs");
        fs::write(
            &src,
            "pub fn outer() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn one() { outer(); }\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .expect("plan should succeed");
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        // Source + target FileEdits in order.
        assert_eq!(plan.edits.len(), 2);
        assert!(plan.edits[0].path.ends_with("java.rs"));
        // Auto-derived: src/refactor/java/tests.rs
        assert!(
            plan.edits[1].path.ends_with("java/tests.rs"),
            "auto-derived target should be java/tests.rs, got {}",
            plan.edits[1].path
        );

        // Source after apply: inline body replaced with `;`, outer attr kept.
        let source_after = apply_to(&plan_json, 0);
        assert!(
            source_after.contains("#[cfg(test)]\nmod tests;"),
            "outer attribute must be preserved on the declaration:\n{source_after}"
        );
        assert!(
            !source_after.contains("mod tests {"),
            "inline block must be replaced: {source_after}"
        );

        // Target body: tests, de-indented one level.
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("use super::*;"),
            "body content must land in target: {target_after}"
        );
        assert!(
            target_after.contains("fn one()"),
            "body content must land in target: {target_after}"
        );
        // De-indented: `use super::*;` at column 0, not column 4.
        assert!(
            target_after.starts_with("use super::*;")
                || target_after.starts_with("\nuse super::*;")
                || target_after.lines().any(|l| l == "use super::*;"),
            "body should be de-indented to module top-level: {target_after}"
        );
    }

    // Gate: explicit target path overrides the auto-derived layout.
    #[test]
    fn honors_explicit_target_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.rs");
        let target = dir.path().join("custom/test_bodies.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&src, "#[cfg(test)]\nmod tests {\n    fn x() {}\n}\n").unwrap();

        let plan_json =
            plan_inline_mod_to_file_submodule(&make_params(&src, "tests", Some(&target)))
                .expect("plan should succeed");
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert!(
            plan.edits[1].path.ends_with("test_bodies.rs"),
            "explicit target should be honored: {}",
            plan.edits[1].path
        );
    }

    // Gate: lib.rs / main.rs / mod.rs put the submodule in the same dir,
    // not a nested directory.
    #[test]
    fn flat_layout_for_root_files() {
        let dir = tempfile::tempdir().unwrap();
        for stem in &["lib", "main", "mod"] {
            let src = dir.path().join(format!("{stem}.rs"));
            fs::write(&src, "mod inner {\n    fn x() {}\n}\n").unwrap();
            let plan_json =
                plan_inline_mod_to_file_submodule(&make_params(&src, "inner", None)).unwrap();
            let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
            let target = &plan.edits[1].path;
            assert!(
                target.ends_with("inner.rs"),
                "{stem}.rs should derive sibling inner.rs target, got {target}",
            );
            assert!(
                !target.contains(&format!("/{stem}/")),
                "{stem}.rs should NOT derive nested {stem}/inner.rs, got {target}",
            );
            fs::remove_file(&src).unwrap();
        }
    }

    // Gate: refuse to extract a mod that's already a file submodule
    // declaration (no body block).
    #[test]
    fn refuses_already_file_submodule() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.rs");
        fs::write(&src, "mod foo;\n").unwrap();
        let err = plan_inline_mod_to_file_submodule(&make_params(&src, "foo", None))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already a file submodule declaration"),
            "expected file-submodule refusal, got: {err}"
        );
    }

    // Gate: refuse to overwrite a non-empty target file.
    #[test]
    fn refuses_non_empty_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.rs");
        let target = dir.path().join("custom_target.rs");
        fs::write(&src, "mod foo {\n    fn x() {}\n}\n").unwrap();
        fs::write(&target, "// pre-existing content\nfn keep() {}\n").unwrap();
        let err = plan_inline_mod_to_file_submodule(&make_params(&src, "foo", Some(&target)))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already exists and is non-empty"),
            "expected non-empty-target refusal, got: {err}"
        );
    }

    // Gate: an empty pre-existing target file (e.g. operator-scaffolded) is
    // accepted and overwritten.
    #[test]
    fn accepts_empty_pre_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.rs");
        let target = dir.path().join("scaffold.rs");
        fs::write(&src, "mod foo {\n    fn x() {}\n}\n").unwrap();
        fs::write(&target, "").unwrap();
        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "foo", Some(&target)))
            .expect("empty target should be accepted");
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(plan.edits.len(), 2);
    }

    // Gate: refuse when the named mod doesn't exist.
    #[test]
    fn refuses_missing_mod() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.rs");
        fs::write(&src, "fn unrelated() {}\n").unwrap();
        let err = plan_inline_mod_to_file_submodule(&make_params(&src, "missing", None))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not found"),
            "expected not-found error, got: {err}"
        );
    }

    // G7: include_str! paths are rebased when a #[cfg(test)] mod moves deeper.
    #[test]
    fn repairs_include_str_when_test_mod_moves_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/module");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("engine.rs");
        fs::write(
            &src,
            "pub fn run() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn it_works() {\n        let data = include_str!(\"../fixtures/test_data.json\");\n    }\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .expect("plan should succeed");
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("include_str!(\"../../fixtures/test_data.json\")"),
            "fixture path should gain one ../ when moving one level deeper:\n{target_after}"
        );
    }

    // G7: include_bytes! and include! are also repaired.
    #[test]
    fn repairs_include_bytes_and_include() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/module");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("engine.rs");
        fs::write(
            &src,
            "#[cfg(test)]\nmod tests {\n    let a = include_bytes!(\"data.bin\");\n    let b = include!(\"helpers.rs\");\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .expect("plan should succeed");
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("include_bytes!(\"../data.bin\")"),
            "include_bytes path should gain ../:\n{target_after}"
        );
        assert!(
            target_after.contains("include!(\"../helpers.rs\")"),
            "include path should gain ../:\n{target_after}"
        );
    }

    // G7: no path repair when mod.rs / lib.rs (same-level target).
    #[test]
    fn no_path_repair_at_same_level() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("mod.rs");
        fs::write(
            &src,
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {\n        let data = include_str!(\"fixtures/test_data.json\");\n    }\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .expect("plan should succeed");
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("include_str!(\"fixtures/test_data.json\")"),
            "fixture path should NOT change at same level:\n{target_after}"
        );
    }

    // G7: reject when include macro has non-string-literal argument.
    #[test]
    fn rejects_non_literal_include_when_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/module");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("engine.rs");
        fs::write(
            &src,
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {\n        let data = include_str!(concat!(env!(\"OUT_DIR\"), \"/test.json\"));\n    }\n}\n",
        )
        .unwrap();

        let err = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("non-string-literal"),
            "expected rejection for non-literal include, got: {err}"
        );
    }

    // G7: non-test modules are not path-repaired even when moving deeper.
    #[test]
    fn no_repair_for_non_test_mod() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/module");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("engine.rs");
        fs::write(
            &src,
            "mod inner {\n    const DATA: &str = include_str!(\"../data.txt\");\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "inner", None))
            .expect("plan should succeed");
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("include_str!(\"../data.txt\")"),
            "non-test mod paths should NOT be repaired:\n{target_after}"
        );
    }

    // G7: absolute paths in include_str! are left alone.
    #[test]
    fn absolute_paths_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src/module");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("engine.rs");
        fs::write(
            &src,
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {\n        let data = include_str!(\"/absolute/path.json\");\n    }\n}\n",
        )
        .unwrap();

        let plan_json = plan_inline_mod_to_file_submodule(&make_params(&src, "tests", None))
            .expect("plan should succeed");
        let target_after = apply_to(&plan_json, 1);
        assert!(
            target_after.contains("include_str!(\"/absolute/path.json\")"),
            "absolute paths should NOT be rebased:\n{target_after}"
        );
    }

    // G7: depth_increase unit tests.
    #[test]
    fn depth_increase_computation() {
        assert_eq!(
            compute_depth_increase(Path::new("/src/foo.rs"), Path::new("/src/tests.rs")),
            0,
            "same level → 0"
        );
        assert_eq!(
            compute_depth_increase(Path::new("/src/foo.rs"), Path::new("/src/foo/tests.rs")),
            1,
            "one level deeper → 1"
        );
        assert_eq!(
            compute_depth_increase(Path::new("/src/foo.rs"), Path::new("/src/foo/bar/tests.rs")),
            2,
            "two levels deeper → 2"
        );
        assert_eq!(
            compute_depth_increase(Path::new("/src/a/b.rs"), Path::new("/src/tests.rs")),
            0,
            "shallower → 0"
        );
    }
}

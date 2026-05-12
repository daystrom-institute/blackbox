//! `move_rust_items_with_callers` plan kind.
//!
//! Rust analog of the cross-file caller rewriter we have for Java
//! (JAVA_TOOL_GAPS Gap 4). When top-level items are moved from one
//! module to another and other files in the project reference those
//! items via the source module's simple name (`mod_a::Item`), this kind
//! produces:
//!
//! - Source FileEdit: deletion of each moved item.
//! - Target FileEdit: append moved item text (visibility-preserved).
//! - Per-caller FileEdit (one per touched file): every occurrence of
//!   `<source_simple>::<moved_name>` is rewritten to
//!   `<target_simple>::<moved_name>` — including occurrences inside
//!   `use` declarations. The rewriter operates on textual prefix
//!   matches with Rust word-boundary checks; it doesn't try to track
//!   re-exports, aliases, or fully-qualified `crate::...::mod_a::...`
//!   paths beyond the simple-name segment match.
//!
//! Inputs:
//! - `source` (file), `target` (file): the move geometry.
//! - `item_names` (required), `item_kinds` (optional).
//! - `module_name`: source module's simple name. Defaults to the source
//!   file stem (with `lib`/`main`/`mod` rejected — explicit override
//!   required because those names rarely match call sites).
//! - `target_prelude`: optional. Used as the "target module simple name"
//!   when set; defaults to the target file stem.
//!   (Yes, we're overloading `target_prelude` — there's no dedicated
//!   parameter for "target module simple name" in `RefactorPlanParams`.
//!   When a richer schema lands, swap it to a dedicated field.)
//! - `project_dir` (required): root for the caller walk. Skips
//!   `target/`, `build/`, `node_modules/`, `.git/`, plus the source
//!   and target files themselves.
//!
//! Limits (v1):
//! - Simple-name segment match only. `crate::foo::source_simple::Item`
//!   gets rewritten; `crate::foo::Item` (where the canonical path
//!   skipped `source_simple`) does not.
//! - No splitting of multi-import use trees. A use tree like
//!   `use foo::{A, B}` where only A moves does NOT split — the prefix
//!   `foo` gets rewritten in place (changing where B resolves too).
//!   Operators should split manually before invoking, or call this
//!   tool per-item.
//! - No alias awareness. `use foo::Item as X;` works the same as the
//!   non-aliased form.

use super::*;
use std::collections::HashSet;

pub(crate) fn plan_move_rust_items_with_callers(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_rust_items_with_callers"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for move_rust_items_with_callers"))?
        .to_string();
    let project_dir = Path::new(&project_dir);

    // Derive module simple names.
    let source_simple = resolve_module_simple_name(
        p.module_name.as_deref(),
        &source_path,
        "source",
    )?;
    let target_simple = resolve_module_simple_name(
        p.target_prelude.as_deref(),
        &target_path,
        "target",
    )?;
    if source_simple == target_simple {
        bail!(
            "source and target module simple names must differ; got `{source_simple}`"
        );
    }

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = select_top_level_items_for_move(
        &items,
        p.item_names.as_deref(),
        p.item_kinds.as_deref(),
    )?;
    let moved_names: HashSet<&str> = selected
        .iter()
        .filter_map(|i| i.name.as_deref())
        .collect();

    // Source-side deletion edits.
    let source_edits: Vec<TextEdit> = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect();
    ensure_non_overlapping(&source_edits)
        .context("move_rust_items_with_callers source delete edits overlap")?;

    // Target-side append (no visibility transformation — caller can chain
    // a `rewrite_rust_item_visibility` if needed).
    let mut moved_text = String::new();
    for item in &selected {
        let text = parsed
            .source
            .get(item.leading_trivia_start..item.byte_end)
            .ok_or_else(|| anyhow!("invalid item range for {}", item.plan_local_id))?
            .trim_matches('\n');
        if !moved_text.is_empty() {
            moved_text.push_str("\n\n");
        }
        moved_text.push_str(text);
        moved_text.push('\n');
    }
    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let target_insert = if target_source.trim().is_empty() {
        moved_text.clone()
    } else if target_source.ends_with('\n') {
        format!("\n{}", moved_text)
    } else {
        format!("\n\n{}", moved_text)
    };
    let target_edit = TextEdit {
        byte_start: target_source.len(),
        byte_end: target_source.len(),
        replacement: target_insert,
    };

    // Caller walk.
    let mut file_edits = vec![
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
    ];
    let canonical_source = fs::canonicalize(&source_path).ok();
    let canonical_target = fs::canonicalize(&target_path).ok();
    for entry in walkdir::WalkDir::new(project_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        if path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("target" | "build" | ".gradle" | "node_modules" | ".git")
            )
        }) {
            continue;
        }
        let canonical = fs::canonicalize(path).ok();
        if canonical.is_some()
            && (canonical == canonical_source || canonical == canonical_target)
        {
            continue;
        }
        let caller_source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let edits = compute_caller_rewrite_edits(
            &caller_source,
            &source_simple,
            &target_simple,
            &moved_names,
        );
        if edits.is_empty() {
            continue;
        }
        file_edits.push(FileEdit {
            path: path_string(path),
            original_sha256: sha256_hex(caller_source.as_bytes()),
            edits,
            new_text: None,
        });
    }

    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .flat_map(|fe| parse_validation_step_for_path(Path::new(&fe.path)))
        .collect();

    let plan = RefactorPlan {
        title: format!(
            "move {} Rust item(s) from {} to {} with caller rewrites",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "move_rust_items_with_callers".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
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
        .context("validate move_rust_items_with_callers plan")?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// Resolve a module's simple name. Explicit override wins. Otherwise
/// fall back to the file stem, rejecting `lib`/`main`/`mod` because
/// those names rarely match caller-side path segments.
fn resolve_module_simple_name(
    explicit: Option<&str>,
    path: &Path,
    label: &str,
) -> Result<String> {
    if let Some(name) = explicit {
        if !name.is_empty() {
            return Ok(name.to_string());
        }
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{label} path has no file stem: {}", path.display()))?;
    if matches!(stem, "lib" | "main" | "mod") {
        bail!(
            "{label} file stem is `{stem}`; pass {label}_module_name explicitly \
             (this kind needs a non-generic module simple name to match callers)"
        );
    }
    Ok(stem.to_string())
}

fn select_top_level_items_for_move<'a>(
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
                "move_rust_items_with_callers does not support impl_method; \
                 use extract_rust_impl_methods"
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

/// Find every occurrence of `<source_simple>::<item>` in `caller_source`
/// where `<item>` is a moved name, and emit a TextEdit replacing the
/// `<source_simple>` prefix with `<target_simple>`. Word-boundary
/// checks prevent matching `not_source_simple::item` or
/// `source_simpleX::item`.
fn compute_caller_rewrite_edits(
    caller_source: &str,
    source_simple: &str,
    target_simple: &str,
    moved_names: &HashSet<&str>,
) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    let bytes = caller_source.as_bytes();
    let source_simple_bytes = source_simple.as_bytes();
    let source_len = source_simple_bytes.len();
    let total = bytes.len();
    let mut i = 0;
    while i + source_len <= total {
        if &bytes[i..i + source_len] != source_simple_bytes {
            i += 1;
            continue;
        }
        // Word boundary before: previous byte must not be an ident byte.
        // A leading `:` (from `path::source_simple::...`) IS valid — the
        // colon is a path separator, not part of the simple name.
        if i > 0 {
            let prev = bytes[i - 1];
            if rust_ident_byte_strict(prev) {
                i += 1;
                continue;
            }
        }
        // Must be followed by `::`.
        let after = i + source_len;
        if after + 2 > total || &bytes[after..after + 2] != b"::" {
            i += 1;
            continue;
        }
        // Scan the next ident segment.
        let ident_start = after + 2;
        let ident_end = scan_ident_end(bytes, ident_start);
        if ident_end == ident_start {
            i = ident_start;
            continue;
        }
        // Must match a moved name.
        let ident = &caller_source[ident_start..ident_end];
        if !moved_names.contains(ident) {
            i = ident_end;
            continue;
        }
        edits.push(TextEdit {
            byte_start: i,
            byte_end: i + source_len,
            replacement: target_simple.to_string(),
        });
        i = ident_end;
    }
    edits
}

fn rust_ident_byte_strict(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn scan_ident_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && rust_ident_byte_strict(bytes[i]) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_params(
        source: &Path,
        target: &Path,
        item_names: &[&str],
        module_name: Option<&str>,
        target_prelude: Option<&str>,
        project_dir: &Path,
    ) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "move_rust_items_with_callers".to_string(),
            source: source.to_string_lossy().into_owned(),
            target: Some(target.to_string_lossy().into_owned()),
            item_names: Some(item_names.iter().map(|s| s.to_string()).collect()),
            item_kinds: None,
            impl_name: None,
            module_name: module_name.map(|s| s.to_string()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: target_prelude.map(|s| s.to_string()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(project_dir.to_string_lossy().into_owned()),
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
            declaring_class: None,
            summary_only: None,
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

    fn find_edit_for(plan: &RefactorPlan, suffix: &str) -> Option<usize> {
        plan.edits.iter().position(|fe| fe.path.ends_with(suffix))
    }

    // Gate: source delete + target append + caller `use mod_a::Item;` rewritten.
    #[test]
    fn rewrites_callers_in_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("mod_a.rs");
        let target = src_dir.join("mod_b.rs");
        let caller = src_dir.join("usage.rs");
        fs::write(&source, "pub fn moved() -> usize { 1 }\n").unwrap();
        fs::write(&target, "").unwrap();
        fs::write(
            &caller,
            "use crate::mod_a::moved;\nfn run() { let _ = moved(); }\n",
        )
        .unwrap();

        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .expect("plan should succeed");
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let caller_idx = find_edit_for(&plan, "usage.rs").expect("usage.rs must appear");
        let rewritten = apply_file_edit(&caller, &plan.edits[caller_idx]);
        assert!(
            rewritten.contains("use crate::mod_b::moved;"),
            "use decl must point at mod_b: {rewritten}"
        );
        assert!(
            !rewritten.contains("mod_a::moved"),
            "old mod_a reference should be gone: {rewritten}"
        );
    }

    // Gate: word-boundary protection — `mod_a_x::moved` does NOT get
    // rewritten because the prefix doesn't end on a word boundary.
    #[test]
    fn skips_word_boundary_false_positives() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("mod_a.rs");
        let target = src_dir.join("mod_b.rs");
        let caller = src_dir.join("other.rs");
        fs::write(&source, "pub fn moved() {}\n").unwrap();
        fs::write(&target, "").unwrap();
        // mod_ax::moved should be left alone.
        fs::write(&caller, "use crate::mod_ax::moved;\n").unwrap();

        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert!(
            find_edit_for(&plan, "other.rs").is_none(),
            "false-positive prefix should not produce a caller edit"
        );
    }

    // Gate: bare path expressions outside `use` decls also get rewritten.
    #[test]
    fn rewrites_bare_path_expressions() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("mod_a.rs");
        let target = src_dir.join("mod_b.rs");
        let caller = src_dir.join("caller.rs");
        fs::write(&source, "pub fn moved() {}\n").unwrap();
        fs::write(&target, "").unwrap();
        fs::write(
            &caller,
            "fn run() {\n    let _ = mod_a::moved();\n    let _ = crate::mod_a::moved;\n}\n",
        )
        .unwrap();
        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let caller_idx = find_edit_for(&plan, "caller.rs").expect("caller.rs must appear");
        let rewritten = apply_file_edit(&caller, &plan.edits[caller_idx]);
        assert!(
            rewritten.contains("mod_b::moved();"),
            "bare path expr should be rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains("crate::mod_b::moved"),
            "qualified path expr should be rewritten: {rewritten}"
        );
        assert!(
            !rewritten.contains("mod_a::"),
            "no mod_a::xxx references should survive: {rewritten}"
        );
    }

    // Gate: rejects `lib`/`main`/`mod` file stems without an explicit override.
    #[test]
    fn rejects_generic_file_stems_without_override() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("lib.rs");
        let target = src_dir.join("mod_b.rs");
        fs::write(&source, "pub fn moved() {}\n").unwrap();
        fs::write(&target, "").unwrap();
        let err = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("lib") && err.contains("module_name"),
            "expected lib-stem rejection, got: {err}"
        );
    }

    // Gate: explicit module_name override is honored.
    #[test]
    fn honors_explicit_module_name() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("a.rs");
        let target = src_dir.join("b.rs");
        let caller = src_dir.join("c.rs");
        fs::write(&source, "pub fn moved() {}\n").unwrap();
        fs::write(&target, "").unwrap();
        // Caller uses module names that match the overrides.
        fs::write(&caller, "use crate::custom_source::moved;\n").unwrap();
        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            Some("custom_source"),
            Some("custom_target"),
            dir.path(),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        let caller_idx = find_edit_for(&plan, "c.rs").expect("c.rs must appear");
        let rewritten = apply_file_edit(&caller, &plan.edits[caller_idx]);
        assert!(
            rewritten.contains("use crate::custom_target::moved;"),
            "explicit module names should drive the rewrite: {rewritten}"
        );
    }

    // Gate: skips source + target files themselves during the walk.
    #[test]
    fn skips_source_and_target_in_walk() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("mod_a.rs");
        let target = src_dir.join("mod_b.rs");
        // Plant a reference to `mod_a::moved` inside source itself —
        // could appear if the moved item had self-references. We skip
        // because source is being deleted-and-rewritten via its own
        // FileEdit; the walker would double-edit otherwise.
        fs::write(
            &source,
            "pub fn moved() { let _ = mod_a::moved as fn(); }\n",
        )
        .unwrap();
        fs::write(&target, "").unwrap();
        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        // Only two FileEdits: source + target. No third entry for source again.
        assert_eq!(
            plan.edits.len(),
            2,
            "source + target only; got {} edits",
            plan.edits.len()
        );
    }

    // Gate: skips target/build/.git directories during the walk.
    #[test]
    fn skips_build_dirs_in_walk() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let build_dir = dir.path().join("target/debug/build");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        let source = src_dir.join("mod_a.rs");
        let target = src_dir.join("mod_b.rs");
        fs::write(&source, "pub fn moved() {}\n").unwrap();
        fs::write(&target, "").unwrap();
        // Reference inside target/ should NOT be touched.
        fs::write(
            build_dir.join("artifact.rs"),
            "use crate::mod_a::moved;\n",
        )
        .unwrap();
        let plan_json = plan_move_rust_items_with_callers(&make_params(
            &source,
            &target,
            &["moved"],
            None,
            None,
            dir.path(),
        ))
        .unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_json).unwrap();
        assert!(
            find_edit_for(&plan, "artifact.rs").is_none(),
            "build-dir file should be skipped"
        );
    }
}

//! `extract_rust_crate` compound plan kind and its sub-primitives
//! (gap-fe4dd97f: monolith → workspace-member crate extraction).
//!
//! The compound mechanizes the "leaf peel with re-export alias" pattern:
//! move one or more root modules of a monolithic crate into a freshly
//! scaffolded workspace-member crate, and replace each origin `mod <m>;`
//! declaration with `use <new_crate>::<m>;` so every existing
//! `crate::<m>::...` call site keeps resolving without a mass path rewrite.
//!
//! Sub-primitives (individually plannable):
//! - `extract_rust_crate_scaffold` — the atomic plan: new-crate Cargo.toml +
//!   lib.rs creates, module file moves, origin alias swap, workspace-members
//!   merge, and consumer dependency wiring, with leaf-coupling preflight.
//! - `rewrite_rust_crate_paths` — `crate::<m>` → `<new_crate>::<m>` rewriting
//!   with mixed-group use-tree splitting (generalizes
//!   `rewrite_rust_bin_crate_paths`, which leaves mixed groups untouched).
//! - `rust_workspace_dag_check` — read-only acyclicity guard over the
//!   workspace's path-dependency graph ([dependencies] + [build-dependencies];
//!   dev-dependency cycles are legal in cargo and excluded).
//!
//! Leaf invariant (fail closed): extraction refuses when moved files still
//! reference `crate::<seg>` for segments that are not moving. The alias trick
//! cannot make those resolve in the new crate, and silently planning them
//! would push the failure into cargo check with a confusing blast radius.
//! Decouple first (move shared helpers down, invert the dependency), then
//! re-plan — the refusal lists every offender with file:line.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::{
    CaptureSpec, FileCreate, FileEdit, FileMove, OnFailure, PlanStatus, RefactorPlan,
    RefactorPlanParams, RefactorRunStep, SemanticStatus, TextEdit, ValidationStep,
    ensure_non_overlapping, path_string, replace_identifier_path_edits, resolve_path,
    rust_path_boundary, sha256_hex, validate_plan_shape, validate_rust_identifier,
};

// ── compound expansion ─────────────────────────────────────────────

/// Expand an `extract_rust_crate` run step into the scaffold + guard +
/// validate pipeline. Mirrors `expand_migrate_rust_mods_to_lib_step`.
pub fn expand_extract_rust_crate_step(
    params: &RefactorPlanParams,
    optional: bool,
    project_dir: &Path,
) -> Result<Vec<RefactorRunStep>> {
    let module_names = params
        .item_names
        .clone()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names (root modules to extract) is required for extract_rust_crate"))?;
    for name in &module_names {
        validate_rust_identifier(name, "item_names")?;
    }
    let crate_name = params
        .module_name
        .clone()
        .ok_or_else(|| anyhow!("module_name (new crate name) is required for extract_rust_crate"))?;
    validate_crate_name(&crate_name)?;
    let project_dir_arg = path_string(project_dir);

    let mut scaffold_params = params.clone();
    scaffold_params.kind = "extract_rust_crate_scaffold".to_string();
    scaffold_params.project_dir = Some(project_dir_arg.clone());

    let mut compile_fix_entries = BTreeMap::new();
    compile_fix_entries.insert(
        "diagnostics_ref".to_string(),
        serde_json::Value::String("last".to_string()),
    );

    Ok(vec![
        RefactorRunStep::Plan {
            params: scaffold_params,
            optional,
        },
        RefactorRunStep::Plan {
            params: RefactorPlanParams {
                kind: "rust_workspace_dag_check".to_string(),
                source: "Cargo.toml".to_string(),
                project_dir: Some(project_dir_arg.clone()),
                ..Default::default()
            },
            optional: false,
        },
        RefactorRunStep::Command {
            command: "cargo".to_string(),
            args: vec![
                "check".to_string(),
                "--workspace".to_string(),
                "--message-format=json".to_string(),
            ],
            cwd: None,
            touches: Vec::new(),
            required: None,
            capture: Some(CaptureSpec::RustcJson),
            on_failure: Some(OnFailure::ContinueForRepair),
        },
        RefactorRunStep::Plan {
            params: RefactorPlanParams {
                kind: "rust_compile_fix_round".to_string(),
                source: String::new(),
                project_dir: Some(project_dir_arg),
                toml_entries: Some(compile_fix_entries),
                ..Default::default()
            },
            optional: true,
        },
        RefactorRunStep::Command {
            command: "cargo".to_string(),
            args: vec!["check".to_string(), "--workspace".to_string()],
            cwd: None,
            touches: Vec::new(),
            required: Some(true),
            capture: None,
            on_failure: None,
        },
    ])
}

// ── scaffold primitive ─────────────────────────────────────────────

pub fn plan_extract_rust_crate_scaffold(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for extract_rust_crate_scaffold"))?;
    let root = resolve_path(None, project_dir)?;
    let module_names = p
        .item_names
        .clone()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names (root modules to extract) is required"))?;
    for name in &module_names {
        validate_rust_identifier(name, "item_names")?;
    }
    let crate_name = p
        .module_name
        .clone()
        .ok_or_else(|| anyhow!("module_name (new crate name) is required"))?;
    validate_crate_name(&crate_name)?;
    let crate_ident = crate_name.replace('-', "_");
    let target_dir_rel = p
        .target
        .clone()
        .unwrap_or_else(|| format!("crates/{crate_name}"));
    let target_dir = root.join(&target_dir_rel);
    if target_dir.exists() {
        bail!(
            "refusing to scaffold {}: directory already exists",
            target_dir.display()
        );
    }
    let origin_lib_rel = if p.source.is_empty() {
        "src/lib.rs".to_string()
    } else {
        p.source.clone()
    };
    let origin_lib_path = root.join(&origin_lib_rel);
    let origin_lib = fs::read_to_string(&origin_lib_path)
        .with_context(|| format!("reading origin lib root {}", origin_lib_path.display()))?;
    let origin_manifest_path = root.join("Cargo.toml");
    let origin_manifest_text = fs::read_to_string(&origin_manifest_path)
        .with_context(|| format!("reading {}", origin_manifest_path.display()))?;

    // 1. Collect the module file trees.
    let mut leftovers = Vec::new();
    let module_set: BTreeSet<&str> = module_names.iter().map(String::as_str).collect();
    let mut moves: Vec<(PathBuf, String)> = Vec::new(); // (abs source, rel under src/)
    let mut module_root_files: Vec<PathBuf> = Vec::new();
    for m in &module_names {
        let file_form = root.join(format!("src/{m}.rs"));
        let dir_form = root.join(format!("src/{m}"));
        match (file_form.is_file(), dir_form.is_dir()) {
            (true, false) => {
                moves.push((file_form.clone(), format!("{m}.rs")));
                module_root_files.push(file_form);
            }
            (false, true) => {
                let mut files = Vec::new();
                collect_rs_files(&dir_form, &mut files)?;
                if files.is_empty() {
                    bail!("module directory src/{m}/ contains no .rs files");
                }
                for f in files {
                    let rel = f
                        .strip_prefix(root.join("src"))
                        .map_err(|_| anyhow!("module file escapes src/: {}", f.display()))?;
                    moves.push((f.clone(), path_string(rel)));
                }
                let mod_rs = dir_form.join("mod.rs");
                if mod_rs.is_file() {
                    module_root_files.push(mod_rs);
                }
                leftovers.push(format!(
                    "src/{m}/ will be left as an empty directory after the move \
                     (apply removes files, not directories); remove it manually"
                ));
            }
            (true, true) => bail!("module `{m}` exists as both src/{m}.rs and src/{m}/"),
            (false, false) => bail!("module `{m}` not found at src/{m}.rs or src/{m}/"),
        }
    }

    // 2. Leaf-coupling preflight over the moved sources.
    let mut offenders = Vec::new();
    let mut moved_text_all = String::new();
    for (abs, rel) in &moves {
        let text = fs::read_to_string(abs)
            .with_context(|| format!("reading module file {}", abs.display()))?;
        scan_foreign_crate_refs(&text, rel, &module_set, &mut offenders);
        if text.contains("env!(\"CARGO_MANIFEST_DIR\")") {
            leftovers.push(format!(
                "src/{rel}: uses env!(\"CARGO_MANIFEST_DIR\") — after extraction it \
                 resolves to {target_dir_rel}, not the repo root; fix fixture paths \
                 (e.g. ../../) or invert the path to the consumer"
            ));
        }
        moved_text_all.push_str(&text);
        moved_text_all.push('\n');
    }
    for root_file in &module_root_files {
        let text = fs::read_to_string(root_file)?;
        for (idx, line) in text.lines().enumerate() {
            if line.starts_with("use super::") {
                offenders.push(format!(
                    "{}:{}: top-level `use super::` escapes the module into the origin \
                     crate root",
                    root_file.display(),
                    idx + 1
                ));
            }
        }
    }
    if !offenders.is_empty() {
        bail!(
            "extract_rust_crate refuses: moved modules still reference origin-crate \
             items the extraction would orphan (leaf invariant). Decouple these first \
             (move shared helpers into a foundation crate or invert the dependency), \
             then re-plan:\n  {}",
            offenders.join("\n  ")
        );
    }

    // 3. Infer the new crate's dependencies from the origin manifest.
    let origin_doc: toml_edit::DocumentMut = origin_manifest_text
        .parse()
        .with_context(|| format!("parsing {}", origin_manifest_path.display()))?;
    let edition = origin_doc
        .get("package")
        .and_then(|pkg| pkg.get("edition"))
        .and_then(|e| e.as_str())
        .unwrap_or("2021")
        .to_string();
    let explicit_deps = p
        .toml_entries
        .as_ref()
        .and_then(|entries| entries.get("dependencies"))
        .and_then(|v| v.as_object())
        .cloned();
    let mut dep_lines = Vec::new();
    let mut dev_dep_lines = Vec::new();
    if let Some(explicit) = explicit_deps {
        for (name, spec) in explicit {
            dep_lines.push(format!("{name} = {}", super::toml_literal(&spec)?));
        }
    } else {
        infer_dep_lines(
            &origin_doc,
            "dependencies",
            &moved_text_all,
            &target_dir_rel,
            &mut dep_lines,
        )?;
        infer_dep_lines(
            &origin_doc,
            "dev-dependencies",
            &moved_text_all,
            &target_dir_rel,
            &mut dev_dep_lines,
        )?;
        scan_unknown_roots(
            &moved_text_all,
            &origin_doc,
            &module_set,
            &crate_ident,
            &mut leftovers,
        );
    }

    // 4. New crate manifest + lib.rs.
    let mut manifest = String::new();
    manifest.push_str(&format!(
        "[package]\nname = \"{crate_name}\"\nversion = \"0.0.1\"\nedition = \"{edition}\"\n\n\
         # Extracted from the origin crate by extract_rust_crate; the origin aliases\n\
         # the moved modules back under their original `crate::<module>` paths.\n\n\
         [dependencies]\n"
    ));
    for line in &dep_lines {
        manifest.push_str(line);
        manifest.push('\n');
    }
    if !dev_dep_lines.is_empty() {
        manifest.push_str("\n[dev-dependencies]\n");
        for line in &dev_dep_lines {
            manifest.push_str(line);
            manifest.push('\n');
        }
    }
    let mut lib_rs = format!(
        "//! {crate_name} — extracted from the origin crate by `extract_rust_crate`.\n\
         //! Modules move verbatim; the origin re-exports them under their original\n\
         //! `crate::<module>` paths.\n\n"
    );
    for m in &module_names {
        lib_rs.push_str(&format!("pub mod {m};\n"));
    }

    // 5. Origin lib root: swap each `mod <m>;` declaration for the alias.
    let mut origin_edits = Vec::new();
    for m in &module_names {
        origin_edits.push(alias_module_decl_edit(&origin_lib, m, &crate_ident)?);
    }
    origin_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&origin_edits)
        .with_context(|| format!("overlapping alias edits in {origin_lib_rel}"))?;

    // 6. Root manifest: workspace-members merge + path dependency on the new crate.
    let mut root_doc = origin_doc.clone();
    merge_workspace_member(&mut root_doc, &target_dir_rel)?;
    insert_path_dependency(&mut root_doc, &crate_name, &target_dir_rel)?;
    let root_manifest_new = root_doc.to_string();

    // 7. Optional extra consumers.
    let mut edits = vec![
        FileEdit {
            path: path_string(&origin_lib_path),
            original_sha256: sha256_hex(origin_lib.as_bytes()),
            edits: origin_edits,
            new_text: None,
        },
        FileEdit {
            path: path_string(&origin_manifest_path),
            original_sha256: sha256_hex(origin_manifest_text.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: origin_manifest_text.len(),
                replacement: root_manifest_new,
            }],
            new_text: None,
        },
    ];
    for consumer_rel in super::toml_str_array(&p.toml_entries, "consumers") {
        let consumer_path = root.join(&consumer_rel);
        let consumer_text = fs::read_to_string(&consumer_path)
            .with_context(|| format!("reading consumer manifest {}", consumer_path.display()))?;
        let mut consumer_doc: toml_edit::DocumentMut = consumer_text
            .parse()
            .with_context(|| format!("parsing {}", consumer_path.display()))?;
        let consumer_dir_rel = Path::new(&consumer_rel)
            .parent()
            .map(path_string)
            .unwrap_or_default();
        let rel_dep_path = relative_path_between(&consumer_dir_rel, &target_dir_rel);
        insert_path_dependency(&mut consumer_doc, &crate_name, &rel_dep_path)?;
        edits.push(FileEdit {
            path: path_string(&consumer_path),
            original_sha256: sha256_hex(consumer_text.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: consumer_text.len(),
                replacement: consumer_doc.to_string(),
            }],
            new_text: None,
        });
    }

    let file_moves = moves
        .iter()
        .map(|(abs, rel)| {
            let bytes = fs::read(abs)?;
            Ok(FileMove {
                source_path: path_string(abs),
                target_path: path_string(&target_dir.join("src").join(rel)),
                original_sha256: sha256_hex(&bytes),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let plan = RefactorPlan {
        title: format!(
            "extract {} into workspace crate {crate_name} ({target_dir_rel})",
            module_names.join(", ")
        ),
        kind: "extract_rust_crate_scaffold".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves,
        file_creates: vec![
            FileCreate {
                path: path_string(&target_dir.join("Cargo.toml")),
                content: manifest,
            },
            FileCreate {
                path: path_string(&target_dir.join("src/lib.rs")),
                content: lib_rs,
            },
        ],
        edits,
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&origin_lib_path),
            byte_range: None,
        }],
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
        operator_opt_outs_used: Vec::new(),
    };
    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ── crate-path rewriter with use-tree splitting ────────────────────

pub fn plan_rewrite_rust_crate_paths(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let module_names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for rewrite_rust_crate_paths"))?;
    for name in module_names {
        validate_rust_identifier(name, "item_names")?;
    }
    let crate_ident = p
        .module_name
        .clone()
        .ok_or_else(|| anyhow!("module_name (replacement crate ident) is required"))?
        .replace('-', "_");
    validate_rust_identifier(&crate_ident, "module_name")?;

    let mut leftovers = Vec::new();
    // Grouped `use crate::{...}` lines first (their spans subsume the plain
    // path matches inside them).
    let mut edits = rewrite_use_groups_with_split(&source, &crate_ident, module_names, &mut leftovers);
    let group_spans: Vec<(usize, usize)> = edits.iter().map(|e| (e.byte_start, e.byte_end)).collect();
    for module_name in module_names {
        let old = format!("crate::{module_name}");
        let new = format!("{crate_ident}::{module_name}");
        for edit in replace_identifier_path_edits(&source, &old, &new) {
            let inside_group = group_spans
                .iter()
                .any(|(s, e)| edit.byte_start >= *s && edit.byte_end <= *e);
            if !inside_group {
                edits.push(edit);
            }
        }
    }
    edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&edits).with_context(|| {
        format!(
            "overlapping rewrite_rust_crate_paths edits in {}",
            source_path.display()
        )
    })?;
    if edits.is_empty() {
        bail!(
            "no crate::<module> references to rewrite in {}",
            source_path.display()
        );
    }

    let plan = RefactorPlan {
        title: format!(
            "rewrite crate paths in {} to {crate_ident}::...",
            path_string(&source_path)
        ),
        kind: "rewrite_rust_crate_paths".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
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
        operator_opt_outs_used: Vec::new(),
    };
    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ── workspace DAG guard ────────────────────────────────────────────

pub fn plan_rust_workspace_dag_check(p: &RefactorPlanParams) -> Result<String> {
    let manifest_rel = if p.source.is_empty() {
        "Cargo.toml"
    } else {
        p.source.as_str()
    };
    let manifest_path = resolve_path(p.project_dir.as_deref(), manifest_rel)?;
    let root = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("workspace manifest has no parent dir"))?
        .to_path_buf();
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", manifest_path.display()))?;

    // Package dirs: the root package (if any) plus every workspace member.
    let mut package_dirs: Vec<PathBuf> = Vec::new();
    if doc.get("package").is_some() {
        package_dirs.push(root.clone());
    }
    if let Some(members) = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for member in members {
            let Some(member) = member.as_str() else { continue };
            if let Some(prefix) = member.strip_suffix("/*") {
                let base = root.join(prefix);
                if base.is_dir() {
                    for entry in fs::read_dir(&base)? {
                        let path = entry?.path();
                        if path.join("Cargo.toml").is_file() {
                            package_dirs.push(path);
                        }
                    }
                }
            } else {
                package_dirs.push(root.join(member));
            }
        }
    }

    // name → (dir, path-dep edges). Dev-dependencies are excluded: cargo
    // permits dev-dep cycles, so they are not part of the acyclicity contract.
    let mut dir_to_name: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut deps_by_name: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut raw: Vec<(String, PathBuf, Vec<PathBuf>)> = Vec::new();
    for dir in &package_dirs {
        let member_manifest = dir.join("Cargo.toml");
        let member_text = fs::read_to_string(&member_manifest)
            .with_context(|| format!("reading {}", member_manifest.display()))?;
        let member_doc: toml_edit::DocumentMut = member_text
            .parse()
            .with_context(|| format!("parsing {}", member_manifest.display()))?;
        let Some(name) = member_doc
            .get("package")
            .and_then(|pkg| pkg.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string)
        else {
            continue; // virtual manifest
        };
        let mut edges = Vec::new();
        for table in ["dependencies", "build-dependencies"] {
            if let Some(deps) = member_doc.get(table).and_then(|d| d.as_table_like()) {
                for (_, spec) in deps.iter() {
                    if let Some(path_str) = dep_spec_path(spec) {
                        edges.push(normalize_path(&dir.join(path_str)));
                    }
                }
            }
        }
        dir_to_name.insert(normalize_path(dir), name.clone());
        raw.push((name, normalize_path(dir), edges));
    }
    let mut edge_count = 0usize;
    for (name, _dir, edges) in &raw {
        let resolved: Vec<(String, PathBuf)> = edges
            .iter()
            .filter_map(|target| {
                dir_to_name
                    .get(target)
                    .map(|n| (n.clone(), target.clone()))
            })
            .collect();
        edge_count += resolved.len();
        deps_by_name.insert(name.clone(), resolved);
    }

    // DFS cycle detection.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit(
        node: &str,
        deps: &BTreeMap<String, Vec<(String, PathBuf)>>,
        marks: &mut BTreeMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let pos = stack.iter().position(|n| n == node).unwrap_or(0);
                let mut cycle = stack[pos..].to_vec();
                cycle.push(node.to_string());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(node.to_string(), Mark::Visiting);
        stack.push(node.to_string());
        if let Some(edges) = deps.get(node) {
            for (next, _) in edges {
                if let Some(cycle) = visit(next, deps, marks, stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        marks.insert(node.to_string(), Mark::Done);
        None
    }
    let mut marks = BTreeMap::new();
    for name in deps_by_name.keys() {
        let mut stack = Vec::new();
        if let Some(cycle) = visit(name, &deps_by_name, &mut marks, &mut stack) {
            bail!(
                "workspace dependency cycle detected: {} (path dependencies must \
                 form a DAG; dev-dependencies were excluded from this check)",
                cycle.join(" -> ")
            );
        }
    }

    let plan = RefactorPlan {
        title: format!(
            "workspace dependency graph is acyclic ({} packages, {} path edges)",
            deps_by_name.len(),
            edge_count
        ),
        kind: "rust_workspace_dag_check".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: Vec::new(),
        validations: Vec::new(),
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
    // Analysis plan: no edits by design, so the writes-required shape check
    // (validate_plan_shape) does not apply — same as the other analysis kinds.
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ── helpers ────────────────────────────────────────────────────────

fn validate_crate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    if !ok {
        bail!(
            "module_name `{name}` is not a valid crate name (lowercase ascii, \
             digits, `-`, `_`; must start with a letter)"
        );
    }
    Ok(())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading module dir {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Record `crate::<seg>` references whose first segment is not in the moved
/// set. Comment-only lines are skipped (a doc reference must not block the
/// extraction); string literals are not parsed — a false positive there
/// surfaces as a refusal the operator can inspect.
fn scan_foreign_crate_refs(
    text: &str,
    rel: &str,
    moved: &BTreeSet<&str>,
    offenders: &mut Vec<String>,
) {
    let mut offset = 0usize;
    for (line_idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if !(trimmed.starts_with("//") || trimmed.starts_with("//!") || trimmed.starts_with("///"))
        {
            let mut start = 0usize;
            while let Some(found) = line[start..].find("crate::") {
                let at = start + found;
                let before = if at == 0 {
                    None
                } else {
                    line.as_bytes().get(at - 1).copied()
                };
                // Skip `::crate::` impossible; skip identifiers like `extracted_crate::`.
                if rust_path_boundary(before) {
                    let rest = &line[at + "crate::".len()..];
                    let seg: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !seg.is_empty() && !moved.contains(seg.as_str()) {
                        offenders.push(format!(
                            "src/{rel}:{}: crate::{seg}",
                            line_idx + 1
                        ));
                    }
                }
                start = at + "crate::".len();
            }
        }
        offset += line.len() + 1;
    }
    let _ = offset;
}

fn infer_dep_lines(
    origin_doc: &toml_edit::DocumentMut,
    table: &str,
    moved_text: &str,
    target_dir_rel: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let Some(deps) = origin_doc.get(table).and_then(|d| d.as_table_like()) else {
        return Ok(());
    };
    for (name, spec) in deps.iter() {
        let ident = name.replace('-', "_");
        if !text_references_root_ident(moved_text, &ident) {
            continue;
        }
        let mut rendered = render_dep_spec(spec)?;
        if let Some(orig_path) = dep_spec_path(spec) {
            let rel = relative_path_between(target_dir_rel, &orig_path);
            rendered = rewrite_rendered_path(&rendered, &rel);
        }
        out.push(format!("{name} = {rendered}"));
    }
    Ok(())
}

/// True when `text` contains `<ident>::` at a path-root position (boundary
/// before, and not preceded by `::` which would make it a nested segment).
fn text_references_root_ident(text: &str, ident: &str) -> bool {
    let needle = format!("{ident}::");
    let mut start = 0usize;
    while let Some(found) = text[start..].find(&needle) {
        let at = start + found;
        let before = if at == 0 {
            None
        } else {
            text.as_bytes().get(at - 1).copied()
        };
        let preceded_by_colons = at >= 2 && &text[at - 2..at] == "::";
        if rust_path_boundary(before) && !preceded_by_colons {
            return true;
        }
        start = at + needle.len();
    }
    false
}

fn scan_unknown_roots(
    moved_text: &str,
    origin_doc: &toml_edit::DocumentMut,
    moved: &BTreeSet<&str>,
    crate_ident: &str,
    leftovers: &mut Vec<String>,
) {
    let mut known: BTreeSet<String> = ["std", "core", "alloc", "crate", "super", "self"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    known.insert(crate_ident.to_string());
    for m in moved {
        known.insert((*m).to_string());
    }
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = origin_doc.get(table).and_then(|d| d.as_table_like()) {
            for (name, _) in deps.iter() {
                known.insert(name.replace('-', "_"));
            }
        }
    }
    let mut unknown = BTreeSet::new();
    for line in moved_text.lines() {
        let trimmed = line.trim_start();
        let rest = if let Some(rest) = trimmed.strip_prefix("pub use ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("use ") {
            rest
        } else {
            continue;
        };
        let root: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !root.is_empty() && !known.contains(&root) {
            unknown.insert(root);
        }
    }
    for root in unknown {
        leftovers.push(format!(
            "use root `{root}` not found in origin [dependencies]; if it is a \
             re-export or renamed dep, add it to the new crate manually \
             (compile-fix round may also catch it)"
        ));
    }
}

fn render_dep_spec(spec: &toml_edit::Item) -> Result<String> {
    if let Some(s) = spec.as_str() {
        return Ok(format!("\"{s}\""));
    }
    if let Some(inline) = spec.as_value() {
        return Ok(inline.to_string().trim().to_string());
    }
    if let Some(table) = spec.as_table() {
        // Regular [dependencies.x] table: render inline.
        let mut parts = Vec::new();
        for (k, v) in table.iter() {
            let v = v
                .as_value()
                .ok_or_else(|| anyhow!("unsupported nested dependency table key {k}"))?;
            parts.push(format!("{k} = {}", v.to_string().trim()));
        }
        return Ok(format!("{{ {} }}", parts.join(", ")));
    }
    bail!("unsupported dependency spec shape")
}

fn dep_spec_path(spec: &toml_edit::Item) -> Option<String> {
    if let Some(inline) = spec.as_value().and_then(|v| v.as_inline_table()) {
        return inline
            .get("path")
            .and_then(|p| p.as_str())
            .map(str::to_string);
    }
    if let Some(table) = spec.as_table() {
        return table
            .get("path")
            .and_then(|p| p.as_str())
            .map(str::to_string);
    }
    None
}

fn rewrite_rendered_path(rendered: &str, new_path: &str) -> String {
    // The rendered spec came from render_dep_spec, so the path value is a
    // simple quoted string we can locate and replace.
    if let Some(start) = rendered.find("path = \"") {
        let value_start = start + "path = \"".len();
        if let Some(end_rel) = rendered[value_start..].find('"') {
            let mut out = String::with_capacity(rendered.len());
            out.push_str(&rendered[..value_start]);
            out.push_str(new_path);
            out.push_str(&rendered[value_start + end_rel..]);
            return out;
        }
    }
    rendered.to_string()
}

/// Relative path from `from_dir` to `to_path`, both repo-root-relative.
fn relative_path_between(from_dir: &str, to_path: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|c| !c.is_empty()).collect();
    let to: Vec<&str> = to_path.split('/').filter(|c| !c.is_empty()).collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from.len() {
        parts.push("..".to_string());
    }
    for seg in &to[common..] {
        parts.push((*seg).to_string());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Add `member` to `[workspace].members`, creating the table/array when
/// absent. No-op when the member is already listed. Preserves the existing
/// multiline layout for appended entries.
fn merge_workspace_member(doc: &mut toml_edit::DocumentMut, member: &str) -> Result<()> {
    if doc.get("workspace").is_none() {
        let mut table = toml_edit::Table::new();
        let mut arr = toml_edit::Array::new();
        arr.push(member);
        table["members"] = toml_edit::value(arr);
        doc["workspace"] = toml_edit::Item::Table(table);
        return Ok(());
    }
    let members = doc["workspace"]["members"]
        .or_insert(toml_edit::value(toml_edit::Array::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("[workspace].members is not an array"))?;
    if members.iter().any(|v| v.as_str() == Some(member)) {
        return Ok(());
    }
    let multiline = members
        .iter()
        .any(|v| v.decor().prefix().and_then(|p| p.as_str()).is_some_and(|p| p.contains('\n')));
    let mut value = toml_edit::Value::from(member);
    if multiline {
        value.decor_mut().set_prefix("\n    ");
    }
    members.push_formatted(value);
    Ok(())
}

/// Add `<name> = { path = "<path>" }` to `[dependencies]`, creating the table
/// when absent. No-op when the dependency already exists.
fn insert_path_dependency(
    doc: &mut toml_edit::DocumentMut,
    name: &str,
    path: &str,
) -> Result<()> {
    let deps = doc["dependencies"]
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[dependencies] is not a table"))?;
    if deps.get(name).is_some() {
        return Ok(());
    }
    let mut spec = toml_edit::InlineTable::new();
    spec.insert("path", path.into());
    deps.insert(name, toml_edit::value(spec));
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn alias_module_decl_edit(origin_lib: &str, module: &str, crate_ident: &str) -> Result<TextEdit> {
    let variants = [
        (format!("pub mod {module};"), format!("pub use {crate_ident}::{module};")),
        (
            format!("pub(crate) mod {module};"),
            format!("pub(crate) use {crate_ident}::{module};"),
        ),
        (format!("mod {module};"), format!("use {crate_ident}::{module};")),
    ];
    let mut offset = 0usize;
    for line in origin_lib.lines() {
        let trimmed = line.trim();
        for (decl, replacement) in &variants {
            if trimmed == decl {
                let decl_start = offset + line.find(decl.as_str()).unwrap_or(0);
                return Ok(TextEdit {
                    byte_start: decl_start,
                    byte_end: decl_start + decl.len(),
                    replacement: replacement.clone(),
                });
            }
        }
        offset += line.len() + 1;
    }
    bail!(
        "could not find `mod {module};` (or pub/pub(crate) variant) as a standalone \
         declaration in the origin lib root; extract_rust_crate only aliases plain \
         module declarations (no #[path] attributes, no inline `mod {{}}` blocks)"
    )
}

/// Rewrite grouped `use crate::{{...}}` imports, splitting mixed groups into
/// a kept `crate::{{...}}` group plus a new `<crate_ident>::...` import.
/// Flat single-level groups only; nested groups are reported as leftovers.
fn rewrite_use_groups_with_split(
    source: &str,
    crate_ident: &str,
    module_names: &[String],
    leftovers: &mut Vec<String>,
) -> Vec<TextEdit> {
    let module_set: BTreeSet<&str> = module_names.iter().map(String::as_str).collect();
    let mut edits = Vec::new();
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        let (vis, rest) = if let Some(rest) = trimmed.strip_prefix("pub use crate::{") {
            ("pub ", rest)
        } else if let Some(rest) = trimmed.strip_prefix("use crate::{") {
            ("", rest)
        } else {
            continue;
        };
        let Some(close) = rest.find('}') else { continue };
        let Some(_semi) = rest[close..].find(';') else { continue };
        let inner = &rest[..close];
        if inner.contains('{') {
            leftovers.push(format!(
                "left nested grouped import `{}` unchanged",
                trimmed.trim_end()
            ));
            continue;
        }
        let entries: Vec<&str> = inner
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect();
        if entries.is_empty() {
            continue;
        }
        let root_of = |entry: &str| -> String {
            entry
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect()
        };
        let (moved, kept): (Vec<&str>, Vec<&str>) = entries
            .iter()
            .partition(|entry| module_set.contains(root_of(entry).as_str()));
        if moved.is_empty() {
            continue;
        }
        let line_content_end = line_start + line.trim_end_matches(['\n', '\r']).len();
        let moved_import = if moved.len() == 1 && !moved[0].contains(' ') {
            format!("{indent}{vis}use {crate_ident}::{};", moved[0])
        } else {
            format!("{indent}{vis}use {crate_ident}::{{{}}};", moved.join(", "))
        };
        let replacement = if kept.is_empty() {
            moved_import
        } else {
            format!(
                "{indent}{vis}use crate::{{{}}};\n{moved_import}",
                kept.join(", ")
            )
        };
        edits.push(TextEdit {
            byte_start: line_start,
            byte_end: line_content_end,
            replacement,
        });
    }
    edits
}

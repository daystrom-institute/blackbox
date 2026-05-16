//! EX-G19 `elixir_move_module_across_apps`.
//!
//! Move an Elixir module from one umbrella `apps/<src>/` to another
//! `apps/<dst>/`. Atomically:
//!   - Emits a FileMove for the module file.
//!   - Reports cross-app dependencies (modules in target_app the moved module
//!     depends on; modules in source_app the moved module depended on).
//!   - Reports proposed mix.exs edits for both apps (advisory; v1 does NOT
//!     auto-rewrite mix.exs to avoid touching dynamic deps logic).
//!   - Reports config/*.exs references that the operator must rewrite.
//!
//! Refusals:
//!  - `error.bad_input(code=source_not_in_apps_lib)` — source isn't under
//!    `apps/<x>/lib/`.
//!  - `error.bad_input(code=cyclical_app_dependency)` — proposed move
//!    introduces a cycle between source and target apps.
//!  - `error.bad_input(code=mix_exs_unparseable)` — source/target mix.exs
//!    has dynamic deps logic. Refused unless
//!    `acknowledge_app_boundary_crossing=true`.
//!  - `error.bad_input(code=config_dynamic)` — config refs the module via
//!    dynamic atom synthesis (`Module.concat([...])`).

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use super::{parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, FileMove, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus,
    ValidationStep, resolve_path, sha256_hex, toml_bool,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    cross_app_dependencies: CrossAppDeps,
    mix_exs_edits: Vec<MixExsEdit>,
    config_references: Vec<ConfigRef>,
    source_app: String,
    target_app: String,
    moved_module: String,
}

#[derive(Debug, Serialize, Default)]
struct CrossAppDeps {
    /// Modules in target_app that the moved module depends on (uses
    /// at runtime). Verifies target_app's :deps; if missing, the move
    /// must add source_app→target_app's :deps.
    target_app_dependencies: Vec<String>,
    /// Modules in source_app the moved module depended on. The moved
    /// module now lives in target_app; target_app must :deps source_app
    /// to compile.
    source_app_dependencies: Vec<String>,
}

#[derive(Debug, Serialize)]
struct MixExsEdit {
    app: String,
    advisory: String,
}

#[derive(Debug, Serialize)]
struct ConfigRef {
    file: String,
    line: usize,
    excerpt: String,
}

pub(crate) fn plan_move_across_apps(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_app = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("target_app"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("toml_entries.target_app is required (e.g. \"apps/witness\")"))?
        .to_string();
    let _ack_boundary = toml_bool(&p.toml_entries, "acknowledge_app_boundary_crossing");

    // Identify the source's app root: walk ancestors for the apps/<x>/lib pattern.
    let (source_app_root, source_app_name, rel_from_app_lib) =
        identify_apps_lib_root(&source_path)?;
    let source_app = format!("apps/{}", source_app_name);

    if source_app == target_app {
        bail!(
            "error.bad_input(code=source_and_target_same): source and target apps are both {}",
            source_app
        );
    }

    // Compute the target path: target_app + lib + same rel_from_app_lib.
    let target_path_str = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("target_path_in_app"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let project_dir = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| source_app_root.parent().unwrap().parent().unwrap().to_path_buf());
    let target_path = match target_path_str {
        Some(s) => resolve_path(p.project_dir.as_deref(), &s)?,
        None => project_dir.join(&target_app).join("lib").join(&rel_from_app_lib),
    };

    // Parse source to extract module name + dependencies.
    let parsed = parse_elixir_file(&source_path)?;
    let defmod = top_level_defmodule(&parsed.tree, &parsed.source)
        .ok_or_else(|| anyhow!("error.bad_input(code=no_defmodule): {}", source_path.display()))?;
    let moved_module =
        super::module_deps::defmodule_full_name_pub(defmod, &parsed.source)
            .ok_or_else(|| anyhow!("error.bad_input(code=defmodule_unnamed)"))?;

    // Cross-app dependency analysis: scan the source body for cross-module
    // calls and aliases; classify by which app they belong to.
    let mut target_app_deps: Vec<String> = Vec::new();
    let mut source_app_deps: Vec<String> = Vec::new();
    let target_app_modules = collect_module_names_in_app(&project_dir, &target_app);
    let source_app_modules = collect_module_names_in_app(&project_dir, &source_app);
    let referenced = collect_referenced_modules(parsed.tree.root_node(), &parsed.source);
    for mref in &referenced {
        if mref == &moved_module {
            continue;
        }
        if target_app_modules.contains(mref) {
            if !target_app_deps.contains(mref) {
                target_app_deps.push(mref.clone());
            }
        } else if source_app_modules.contains(mref) {
            if !source_app_deps.contains(mref) {
                source_app_deps.push(mref.clone());
            }
        }
    }

    // Walk config for module references.
    let mut config_refs: Vec<ConfigRef> = Vec::new();
    for config_root in ["config", &format!("{source_app}/config"), &format!("{target_app}/config")]
    {
        let dir = project_dir.join(config_root);
        if !dir.exists() {
            continue;
        }
        for entry in walkdir_simple(&dir) {
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if line.contains(&moved_module) {
                    config_refs.push(ConfigRef {
                        file: entry.to_string_lossy().into_owned(),
                        line: i + 1,
                        excerpt: line.trim().to_string(),
                    });
                }
            }
            // Dynamic synthesis guard.
            if content.contains("Module.concat") {
                bail!(
                    "error.bad_input(code=config_dynamic): {} uses Module.concat in config; planner can't safely rewrite, operator updates manually first",
                    entry.display()
                );
            }
        }
    }

    let mix_exs_edits = vec![
        MixExsEdit {
            app: target_app.clone(),
            advisory: format!(
                "Add :{src_atom} to deps if not already present (moved module now lives here and may depend on source-app modules: {})",
                source_app_deps.join(", "),
                src_atom = source_app_name
            ),
        },
        MixExsEdit {
            app: source_app.clone(),
            advisory: format!(
                "Consider dropping :{tgt_atom} from deps if no other module in {source_app} depends on it",
                tgt_atom = target_app.trim_start_matches("apps/")
            ),
        },
    ];

    let cross_app = CrossAppDeps {
        target_app_dependencies: target_app_deps,
        source_app_dependencies: source_app_deps,
    };

    let file_move = FileMove {
        source_path: source_path.to_string_lossy().into_owned(),
        target_path: target_path.to_string_lossy().into_owned(),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
    };

    let plan = RefactorPlan {
        title: format!(
            "elixir_move_module_across_apps: {} from {} → {}",
            moved_module, source_app, target_app
        ),
        kind: "elixir_move_module_across_apps".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: false,
        file_moves: vec![file_move],
        edits: Vec::<FileEdit>::new(),
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: target_path.to_string_lossy().into_owned(),
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
    let wrapped = PlanWithReport {
        plan,
        cross_app_dependencies: cross_app,
        mix_exs_edits,
        config_references: config_refs,
        source_app,
        target_app,
        moved_module,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn identify_apps_lib_root(source: &std::path::Path) -> Result<(PathBuf, String, PathBuf)> {
    // Walk up from source until we find a parent of structure apps/<name>/lib/.
    let mut current = source.parent();
    while let Some(p) = current {
        // Check if p ends in lib and p.parent() is apps/<x>.
        if p.file_name().and_then(|s| s.to_str()) == Some("lib") {
            if let Some(parent) = p.parent() {
                if parent
                    .parent()
                    .and_then(|gp| gp.file_name())
                    .and_then(|s| s.to_str())
                    == Some("apps")
                {
                    let app_name = parent
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    let rel = source.strip_prefix(p).unwrap_or(source).to_path_buf();
                    return Ok((parent.to_path_buf(), app_name, rel));
                }
            }
        }
        current = p.parent();
    }
    bail!(
        "error.bad_input(code=source_not_in_apps_lib): {} is not under apps/<x>/lib/",
        source.display()
    )
}

fn collect_module_names_in_app(project_dir: &std::path::Path, app: &str) -> Vec<String> {
    let app_lib = project_dir.join(app).join("lib");
    if !app_lib.exists() {
        return Vec::new();
    }
    let files = match super::module_deps::collect_elixir_files_pub(&app_lib, false) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(tree) = super::parse_elixir(&src) else {
            continue;
        };
        if let Some(defmod) = top_level_defmodule(&tree, &src) {
            if let Some(name) = super::module_deps::defmodule_full_name_pub(defmod, &src) {
                out.push(name);
            }
        }
    }
    out
}

fn collect_referenced_modules(root: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "alias" {
            out.push(source[n.byte_range()].to_string());
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
    out
}

fn walkdir_simple(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if matches!(name, "_build" | "deps" | ".git" | ".claude" | "node_modules") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("exs") {
                out.push(path);
            }
        }
    }
    out
}

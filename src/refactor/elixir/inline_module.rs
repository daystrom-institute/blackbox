//! EX-G16 `inline_elixir_module`.
//!
//! Take a small module file (`source`) and inline its content into a
//! designated `target` file as a private section (preserving its
//! `defmodule X do ... end` wrapper so callers via `X.fn` still work, OR
//! optionally as a flat inline). v1 keeps the wrapper — it's the safer
//! default and avoids name-collision concerns.
//!
//! Inputs: `source` (module to inline), `target` (file receiving the
//! inlined module).
//!
//! Refusals:
//!  - `error.bad_input(code=module_is_struct_carrier)` — source defines
//!    `defstruct`; inlining would change struct-type identity (the
//!    `defstruct`'s parent module is fingerprinted; moving it changes
//!    `Module.from_struct`).
//!  - `error.bad_input(code=module_is_behaviour)` — source declares
//!    `@behaviour`. (We avoid hiding behaviours inside another module's
//!    nested scope.)
//!  - `error.bad_input(code=module_has_compile_callbacks)` — source has
//!    `@before_compile` or `@after_compile`.

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use super::{call_target_name, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    inlined_module: String,
    target_module: String,
}

pub(crate) fn plan_inline_module(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for inline_elixir_module"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if !target_path.exists() {
        bail!(
            "error.bad_input(code=target_not_found): {}",
            target_path.display()
        );
    }

    // ── parse source ──────────────────────────────────────────────────────────
    let source = parse_elixir_file(&source_path)?;
    let source_defmod = top_level_defmodule(&source.tree, &source.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {}",
            source_path.display()
        )
    })?;
    let source_body = defmodule_body_statements(source_defmod, &source.source);

    // Refusals.
    for stmt in &source_body {
        let Some(name) = call_target_name(*stmt, &source.source) else {
            if stmt.kind() == "unary_operator" {
                let mut c = stmt.walk();
                if let Some(inner) = stmt.named_children(&mut c).next() {
                    if let Some(attr) = call_target_name(inner, &source.source) {
                        if matches!(attr, "behaviour") {
                            bail!(
                                "error.bad_input(code=module_is_behaviour): source declares @behaviour"
                            );
                        }
                        if matches!(attr, "before_compile" | "after_compile") {
                            bail!(
                                "error.bad_input(code=module_has_compile_callbacks): source uses @{}",
                                attr
                            );
                        }
                    }
                }
            }
            continue;
        };
        if name == "defstruct" {
            bail!("error.bad_input(code=module_is_struct_carrier): source defines defstruct");
        }
    }

    // ── parse target ─────────────────────────────────────────────────────────
    let target = parse_elixir_file(&target_path)?;
    let target_defmod = top_level_defmodule(&target.tree, &target.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=target_no_defmodule): {}",
            target_path.display()
        )
    })?;
    let source_module_name =
        super::module_deps::defmodule_full_name_pub(source_defmod, &source.source)
            .unwrap_or_else(|| "Inlined".to_string());
    let target_module_name =
        super::module_deps::defmodule_full_name_pub(target_defmod, &target.source)
            .unwrap_or_else(|| "Target".to_string());

    // Inject the source's entire defmodule (verbatim) before the target's
    // closing `end`.
    let insertion = source_defmodule_block_text(&source.source, source_defmod);
    let insert_at = target_defmodule_body_end(target_defmod, &target.source);

    let target_edit = TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement: format!("\n  {}\n", insertion.replace('\n', "\n  ")),
    };

    // Clear the source file (truncate to a comment marker so callers' git
    // history still references it; operator removes the file in a follow-up).
    let source_edit = TextEdit {
        byte_start: 0,
        byte_end: source.source.len(),
        replacement: format!(
            "# Inlined into {target_module_name}; this file is now empty.\n# Delete the file in a follow-up commit.\n"
        ),
    };

    // EX-V6 v1 floor: verify the post-edit target parses cleanly. (The source
    // stub is just comments and trivially valid.)
    super::roundtrip::verify_edits_parse_clean(&target.source, std::slice::from_ref(&target_edit))?;

    let plan = RefactorPlan {
        title: format!(
            "inline_elixir_module: {} → {}",
            source_module_name, target_module_name
        ),
        kind: "inline_elixir_module".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![
            FileEdit {
                path: source_path.to_string_lossy().into_owned(),
                original_sha256: sha256_hex(source.source.as_bytes()),
                edits: vec![source_edit],
                new_text: None,
            },
            FileEdit {
                path: target_path.to_string_lossy().into_owned(),
                original_sha256: sha256_hex(target.source.as_bytes()),
                edits: vec![target_edit],
                new_text: None,
            },
        ],
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
        inlined_module: source_module_name,
        target_module: target_module_name,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn source_defmodule_block_text(source: &str, defmod_call: tree_sitter::Node<'_>) -> String {
    source[defmod_call.byte_range()].to_string()
}

fn target_defmodule_body_end(defmod_call: tree_sitter::Node<'_>, source: &str) -> usize {
    let Some(do_block) = super::call_do_block(defmod_call) else {
        return defmod_call.end_byte();
    };
    let end_byte = do_block.end_byte();
    let bytes = source.as_bytes();
    let mut idx = end_byte.saturating_sub(3);
    while idx > 0 && bytes[idx - 1] != b'\n' && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    idx
}

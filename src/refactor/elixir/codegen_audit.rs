//! EX-G10 `elixir_codegen_audit`.
//!
//! Analysis-only. For a module containing compile-time codegen
//! (`quote do defmodule unquote(name) do ... end end`), this plan kind
//! reports the codegen sites and their inputs surface. It does NOT expand
//! the codegen — that requires the AST helper escript (v2 milestone). v1
//! reports the structural fact so operators have visibility into which
//! parts of the module compile to generated code.
//!
//! Output:
//!   - codegen_sites: list of `{line, kind, header_excerpt}` where `kind`
//!     is `defmodule_codegen` (a `quote do defmodule unquote(...) do`)
//!     or `quote_block` (any other top-level quote).
//!
//! Future v2: integrate the escript helper to fully expand a sample input
//! and emit a snapshot file (`priv/codegen_snapshots/<id>.exs`).

use anyhow::{Result, anyhow};
use serde::Serialize;

use super::{call_target_name, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, ValidationStep,
    resolve_path,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    codegen_sites: Vec<CodegenSite>,
    snapshot_dir: String,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CodegenSite {
    line: usize,
    kind: String,
    header_excerpt: String,
}

pub(crate) fn plan_codegen_audit(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {}",
            source_path.display()
        )
    })?;
    let body = defmodule_body_statements(defmodule, &parsed.source);

    let mut sites: Vec<CodegenSite> = Vec::new();
    let mut stack: Vec<tree_sitter::Node<'_>> = body.to_vec();
    while let Some(n) = stack.pop() {
        if n.kind() == "call" && call_target_name(n, &parsed.source) == Some("quote") {
            // Detect inner `defmodule unquote(...)` pattern.
            let txt = &parsed.source[n.byte_range()];
            let kind = if txt.contains("defmodule") && txt.contains("unquote") {
                "defmodule_codegen"
            } else {
                "quote_block"
            };
            let (line, _) = super::byte_to_line_col(&parsed.source, n.start_byte());
            let header_excerpt: String = txt.lines().take(2).collect::<Vec<_>>().join(" / ");
            sites.push(CodegenSite {
                line,
                kind: kind.to_string(),
                header_excerpt,
            });
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }

    let snapshot_dir = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("snapshot_dir"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "priv/codegen_snapshots".to_string());

    let plan = RefactorPlan {
        title: format!("elixir_codegen_audit: {}", source_path.display()),
        kind: "elixir_codegen_audit".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: Vec::<FileEdit>::new(),
        validations: Vec::<ValidationStep>::new(),
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
        codegen_sites: sites,
        snapshot_dir,
        notes: vec![
            "v1 reports codegen sites; full expansion+snapshot ships with the AST helper escript (v2).".to_string(),
        ],
    };
    Ok(serde_json::to_string(&wrapped)?)
}

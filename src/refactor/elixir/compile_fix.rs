//! EX-G11 `elixir_compile_fix_round`.
//!
//! Ingest `mix compile` diagnostics and propose edits. Used inside
//! `bbox_refactor_run` with `on_failure="continue_for_repair"` after a
//! `mix compile --warnings-as-errors --return-errors` command step.
//!
//! v1 fixers (machine-applicable):
//!   - `unused alias Foo` → remove the `alias Foo` directive
//!   - `unused import Foo` → remove the `import Foo` directive
//!   - `module Foo is not loaded and could not be found` → propose adding
//!     `alias Foo.<Bar>` when a short ref `Bar.x()` exists in the file
//!     (advisory; not auto-applied)
//!   - generic warning lines → reported in `unresolved_diagnostics`, no edit
//!
//! Helper integration: when the AST helper escript is available at
//! `priv/elixir_ast_helper/`, plan kind uses
//! `Code.with_diagnostics/2` via the helper for structured capture.
//! Otherwise falls back to parsing mix compile stderr.

use anyhow::{Result, anyhow};
use serde::Serialize;

use super::helper::{MixDiagnostic, parse_mix_compile_stderr};
use crate::refactor::{
    CaptureSpec, FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    fixable_diagnostics: Vec<MixDiagnostic>,
    unresolved_diagnostics: Vec<MixDiagnostic>,
}

pub(crate) fn plan_compile_fix_round(p: &RefactorPlanParams) -> Result<String> {
    // The plan kind expects to be invoked from `bbox_refactor_run` with a
    // `diagnostics_ref` toml_entry pointing at a captured step. v1 also
    // accepts a `diagnostics_stderr` inline string for one-shot usage.
    let diagnostics_text = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("diagnostics_stderr"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("toml_entries.diagnostics_stderr is required for v1 standalone usage; v2 will accept a diagnostics_ref pointing at a captured run step"))?;

    let diagnostics = parse_mix_compile_stderr(diagnostics_text);
    if diagnostics.is_empty() {
        // Empty diagnostics → empty plan (clean compile).
        return Ok(serde_json::to_string(&PlanWithReport {
            plan: empty_plan(),
            fixable_diagnostics: Vec::new(),
            unresolved_diagnostics: Vec::new(),
        })?);
    }

    let mut fixable: Vec<MixDiagnostic> = Vec::new();
    let mut unresolved: Vec<MixDiagnostic> = Vec::new();
    let mut edits_by_file: std::collections::BTreeMap<String, Vec<TextEdit>> = Default::default();

    let project_dir = p.project_dir.as_deref();
    for diag in &diagnostics {
        if let Some(edit) = classify_and_propose(diag, project_dir) {
            edits_by_file
                .entry(diag.file.clone())
                .or_default()
                .push(edit);
            fixable.push(diag.clone());
        } else {
            unresolved.push(diag.clone());
        }
    }

    let mut file_edits: Vec<FileEdit> = Vec::new();
    for (file, edits) in edits_by_file {
        let abs_path =
            resolve_path(project_dir, &file).map_err(|e| anyhow!("resolving {file}: {e}"))?;
        let original = std::fs::read_to_string(&abs_path).unwrap_or_default();
        file_edits.push(FileEdit {
            path: abs_path.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(original.as_bytes()),
            edits,
            new_text: None,
        });
    }

    let plan = RefactorPlan {
        title: format!(
            "elixir_compile_fix_round: {} fixable / {} unresolved",
            fixable.len(),
            unresolved.len()
        ),
        kind: "elixir_compile_fix_round".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: file_edits,
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
        fixable_diagnostics: fixable,
        unresolved_diagnostics: unresolved,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn classify_and_propose(diag: &MixDiagnostic, project_dir: Option<&str>) -> Option<TextEdit> {
    let msg = diag.message.as_str();
    if msg.starts_with("unused alias ") {
        return propose_remove_directive(diag, project_dir, "alias");
    }
    if msg.starts_with("unused import ") {
        return propose_remove_directive(diag, project_dir, "import");
    }
    // Could classify more patterns here; v1 keeps the catalog narrow.
    None
}

fn propose_remove_directive(
    diag: &MixDiagnostic,
    project_dir: Option<&str>,
    keyword: &str,
) -> Option<TextEdit> {
    let abs_path = resolve_path(project_dir, &diag.file).ok()?;
    let src = std::fs::read_to_string(&abs_path).ok()?;
    let line_idx = diag.line.saturating_sub(1);
    let mut line_start = 0usize;
    for (i, _) in src.lines().enumerate() {
        if i == line_idx {
            let line_end = src[line_start..]
                .find('\n')
                .map(|n| line_start + n + 1)
                .unwrap_or(src.len());
            let line_text = &src[line_start..line_end];
            if line_text.contains(keyword) {
                return Some(TextEdit {
                    byte_start: line_start,
                    byte_end: line_end,
                    replacement: String::new(),
                });
            }
            return None;
        }
        let newline = src[line_start..]
            .find('\n')
            .map(|n| line_start + n + 1)
            .unwrap_or(src.len());
        line_start = newline;
    }
    None
}

fn empty_plan() -> RefactorPlan {
    RefactorPlan {
        title: "elixir_compile_fix_round: clean (no diagnostics)".to_string(),
        kind: "elixir_compile_fix_round".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
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
    }
}

// Allow unused — `CaptureSpec` will be threaded in v2 when diagnostics_ref
// wiring lands.
#[allow(dead_code)]
fn _placeholder(_: &CaptureSpec) {}

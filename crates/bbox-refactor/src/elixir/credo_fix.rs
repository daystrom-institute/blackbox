//! EX-G12 `elixir_credo_fix_round`.
//!
//! Ingest `mix credo --format=json` output and propose machine-applicable
//! edits for the subset of Credo lints whose fix is mechanical and safe.
//! Composes inside `bbox_refactor_run` with
//! `on_failure="continue_for_repair"` after a `mix credo` capture step.
//!
//! v1 machine-applicable lints:
//!  - `Credo.Check.Consistency.SpaceAroundOperators` → format_check fix
//!  - `Credo.Check.Readability.AliasOrder` → defer to elixir_organize_aliases
//!  - `Credo.Check.Readability.UnusedAlias` → remove the alias line
//!  - `Credo.Check.Warning.UnusedEnumOperation` → none (operator decision)
//!  - everything else → reported in `unresolved_issues`
//!
//! Credo's JSON format is stable across versions (Credo owns the format).

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex,
};

#[derive(Debug, Deserialize)]
struct CredoOutput {
    issues: Vec<CredoIssue>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CredoIssue {
    category: Option<String>,
    check: Option<String>,
    filename: Option<String>,
    line_no: Option<usize>,
    column: Option<usize>,
    message: Option<String>,
    priority: Option<i64>,
}

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    fixable_issues: Vec<CredoIssue>,
    unresolved_issues: Vec<CredoIssue>,
}

pub(crate) fn plan_credo_fix_round(p: &RefactorPlanParams) -> Result<String> {
    let json_text = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("credo_json"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!("toml_entries.credo_json is required (mix credo --format=json stdout)")
        })?;
    let output: CredoOutput = serde_json::from_str(json_text)
        .map_err(|e| anyhow!("error.bad_input(code=credo_json_invalid): {e}"))?;

    let mut fixable: Vec<CredoIssue> = Vec::new();
    let mut unresolved: Vec<CredoIssue> = Vec::new();
    let mut edits_by_file: std::collections::BTreeMap<String, Vec<TextEdit>> = Default::default();

    let project_dir = p.project_dir.as_deref();
    for issue in &output.issues {
        match classify_issue(issue, project_dir) {
            Some(edit) => {
                let filename = issue.filename.clone().unwrap_or_default();
                edits_by_file.entry(filename).or_default().push(edit);
                fixable.push(issue.clone());
            }
            None => unresolved.push(issue.clone()),
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
            "elixir_credo_fix_round: {} fixable / {} unresolved",
            fixable.len(),
            unresolved.len()
        ),
        kind: "elixir_credo_fix_round".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
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
        operator_opt_outs_used: Vec::new(),
    };
    Ok(serde_json::to_string(&PlanWithReport {
        plan,
        fixable_issues: fixable,
        unresolved_issues: unresolved,
    })?)
}

fn classify_issue(issue: &CredoIssue, project_dir: Option<&str>) -> Option<TextEdit> {
    let check = issue.check.as_deref()?;
    let filename = issue.filename.as_deref()?;
    let line_no = issue.line_no?;
    if check.ends_with("UnusedAlias") {
        return remove_directive_line(filename, line_no, project_dir, "alias");
    }
    if check.ends_with("UnusedImport") {
        return remove_directive_line(filename, line_no, project_dir, "import");
    }
    None
}

fn remove_directive_line(
    filename: &str,
    line_no: usize,
    project_dir: Option<&str>,
    keyword: &str,
) -> Option<TextEdit> {
    let abs_path = resolve_path(project_dir, filename).ok()?;
    let src = std::fs::read_to_string(&abs_path).ok()?;
    let line_idx = line_no.saturating_sub(1);
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

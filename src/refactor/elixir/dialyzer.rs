//! EX-G13 `elixir_dialyzer_attribution`.
//!
//! Parse `mix dialyzer --format=short` output, map warnings to defs by line,
//! propose @spec narrowing or unreachable-clause removal where mechanical.
//! Dialyzer is success-typing: most warnings are "this can never succeed"
//! → contract narrowing rather than code change. The plan kind classifies:
//!
//!  - `no_return` on a function that does return → narrow @spec
//!  - `extra_range` → narrow @spec return type
//!  - `call_to_missing` → propose alias/require add
//!  - `pattern_match_cov` → remove unreachable clause (advisory; needs human review)
//!  - everything else → unresolved
//!
//! v1: report-only for most categories; mechanical fixes are limited to
//! preserved-shape edits (e.g., precise removal of declared-unreachable
//! clauses by line) that the operator opts into via `apply: true`.
//!
//! Dialyzer's short format:
//! ```text
//! lib/foo.ex:42:Function bar/2 has no local return.
//! lib/foo.ex:50:The pattern can never match the type ...
//! ```

use anyhow::{Result, anyhow};
use serde::Serialize;

use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, ValidationStep,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DialyzerWarning {
    pub file: String,
    pub line: usize,
    pub message: String,
    pub category: String,
}

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    warnings: Vec<DialyzerWarning>,
    categorized: std::collections::BTreeMap<String, usize>,
    unactionable_warnings: Vec<DialyzerWarning>,
}

pub(crate) fn plan_dialyzer_attribution(p: &RefactorPlanParams) -> Result<String> {
    let text = p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("dialyzer_short"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!("toml_entries.dialyzer_short is required (mix dialyzer --format=short stdout)")
        })?;
    let warnings = parse_dialyzer_short(text);
    let mut categorized: std::collections::BTreeMap<String, usize> = Default::default();
    for w in &warnings {
        *categorized.entry(w.category.clone()).or_default() += 1;
    }
    // v1: all warnings are advisory. The catalog of mechanical fixes is
    // intentionally narrow because success-typing warnings often have
    // ambiguous remediation.
    let unactionable = warnings.clone();

    let plan = RefactorPlan {
        title: format!("elixir_dialyzer_attribution: {} warnings", warnings.len()),
        kind: "elixir_dialyzer_attribution".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
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
    Ok(serde_json::to_string(&PlanWithReport {
        plan,
        warnings,
        categorized,
        unactionable_warnings: unactionable,
    })?)
}

fn parse_dialyzer_short(text: &str) -> Vec<DialyzerWarning> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(first_colon) = line.find(':') else {
            continue;
        };
        let after_file = &line[first_colon + 1..];
        let Some(second_colon) = after_file.find(':') else {
            continue;
        };
        let line_num_str = &after_file[..second_colon];
        let Ok(line_num) = line_num_str.parse::<usize>() else {
            continue;
        };
        let file = line[..first_colon].to_string();
        let message = after_file[second_colon + 1..].trim().to_string();
        let category = classify_message(&message);
        out.push(DialyzerWarning {
            file,
            line: line_num,
            message,
            category,
        });
    }
    out
}

fn classify_message(msg: &str) -> String {
    let lower = msg.to_lowercase();
    if lower.contains("has no local return") || lower.contains("no_return") {
        "no_return".to_string()
    } else if lower.contains("the pattern can never match") {
        "pattern_match_cov".to_string()
    } else if lower.contains("function") && lower.contains("undefined") {
        "call_to_missing".to_string()
    } else if lower.contains("the type") && lower.contains("is not used") {
        "unused_type".to_string()
    } else if lower.contains("contract") {
        "extra_range".to_string()
    } else {
        "other".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_dialyzer_short() {
        let input = "lib/foo.ex:42:Function bar/2 has no local return.\nlib/baz.ex:10:The pattern can never match the type :nil\n";
        let w = parse_dialyzer_short(input);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, "no_return");
        assert_eq!(w[1].category, "pattern_match_cov");
    }
}

use anyhow::Result;
use serde_json::Value;

use super::OpEffect;
use super::pathology_common::{
    NormalizeShape, PlanShape, normalize_atom_requests, write_pathology_plan,
};

/// Default perf detector allowlist when a workflow does not pass `allowed_atoms`
/// explicitly. The v0 set covers the ten smells in
/// `design/refactor-tools/perf-pathology.md` plus the runtime-evidence
/// cross-cut. The workflow passes its own allowlist through
/// `args.allowed_atoms`; this is the fallback.
const PERF_DEFAULT_ALLOWED_ATOMS: &[&str] = &[
    "atom:perf-nested-iteration-complexity@v1",
    "atom:perf-eager-materialization@v1",
    "atom:perf-n-plus-one-fetch@v1",
    "atom:perf-blocking-and-sequential-async@v1",
    "atom:perf-unbounded-growth@v1",
    "atom:perf-runtime-evidence-corroboration@v1",
];

/// Perf carries `hot_paths` and `baseline_refs` (operator-supplied runtime
/// evidence) where arch carries `target_loci` / `layer_model_path` /
/// `whole_project_mode`. `survey_json` is special-cased by the shared helper.
const PERF_INHERIT_KEYS: &[&str] = &[
    "project_dir",
    "scope_filter",
    "hot_paths",
    "operator_hints",
    "baseline_refs",
    "target_context_window",
    "whiteboard_id",
];

const PERF_ARRAY_FIELDS: &[&str] = &["hot_paths", "operator_hints", "baseline_refs"];

pub(super) fn exec_normalize_perf_pathology_atom_requests(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    normalize_atom_requests(
        args,
        into_var,
        &NormalizeShape {
            op_label: "NormalizePerfPathologyAtomRequests",
            default_allowed_atoms: PERF_DEFAULT_ALLOWED_ATOMS,
            inherit_keys: PERF_INHERIT_KEYS,
            array_fields: PERF_ARRAY_FIELDS,
        },
    )
}

pub(super) fn exec_write_perf_pathology_plan(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    write_pathology_plan(
        args,
        into_var,
        &PlanShape {
            op_label: "WritePerfPathologyPlan",
            out_subdir: &["design", "refactor", "perf", "plans"],
            plan_kind: "performance-correction-plan",
            topic_tag: "performance",
            default_title_prefix: "Performance Correction Plan",
            default_brief: "Performance pathology correction plan.",
            slug_fallback: "performance-correction-plan",
            default_generated_by: "perf-pathology",
            default_criteria_prefix: "PP",
        },
    )
}

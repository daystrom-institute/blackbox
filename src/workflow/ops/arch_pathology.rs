use anyhow::Result;
use serde_json::Value;

use super::OpEffect;
use super::pathology_common::{NormalizeShape, PlanShape, normalize_atom_requests, write_pathology_plan};

/// Default allowlist when a workflow does not pass `allowed_atoms` explicitly
/// (the Java architecture-pathology survey set). The Rust workflow passes its
/// own allowlist through `args.allowed_atoms`.
const ARCH_DEFAULT_ALLOWED_ATOMS: &[&str] = &[
    "atom:java-architecture-role-behavior-coherence@v1",
    "atom:java-architecture-responsibility-bleed@v1",
    "atom:java-architecture-conceptual-duplicate-discovery@v1",
    "atom:java-architecture-anemic-data-remote-behavior@v1",
    "atom:java-architecture-scoped-context-capture@v1",
    "atom:java-architecture-framework-contract-violation@v1",
    "atom:java-architecture-test-implied-architecture@v1",
    "atom:java-architecture-transcript-anchored-pressure@v1",
];

const ARCH_INHERIT_KEYS: &[&str] = &[
    "project_dir",
    "scope_filter",
    "target_loci",
    "operator_hints",
    "layer_model_path",
    "target_context_window",
    "whole_project_mode",
    "whiteboard_id",
];

const ARCH_ARRAY_FIELDS: &[&str] = &["target_loci", "operator_hints"];

pub(super) fn exec_normalize_arch_pathology_atom_requests(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    normalize_atom_requests(
        args,
        into_var,
        &NormalizeShape {
            op_label: "NormalizeArchPathologyAtomRequests",
            default_allowed_atoms: ARCH_DEFAULT_ALLOWED_ATOMS,
            inherit_keys: ARCH_INHERIT_KEYS,
            array_fields: ARCH_ARRAY_FIELDS,
        },
    )
}

pub(super) fn exec_write_arch_pathology_plan(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    write_pathology_plan(
        args,
        into_var,
        &PlanShape {
            op_label: "WriteArchPathologyPlan",
            out_subdir: &["design", "refactor", "plans"],
            plan_kind: "correction-plan",
            topic_tag: "architecture",
            default_title_prefix: "Architecture Correction Plan",
            default_brief: "Architecture pathology correction plan.",
            slug_fallback: "architecture-correction-plan",
            default_generated_by: "arch-pathology",
            default_criteria_prefix: "AP",
        },
    )
}

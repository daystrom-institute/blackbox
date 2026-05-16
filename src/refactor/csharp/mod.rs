//! C# refactor track.
//!
//! Plan kinds, semantic tiers, and atom-facing primitives mirror the
//! Rust and Java tracks. See `design/refactor-tools/csharp/refactor-csharp-expansion.md`
//! for the full contract; cross-cutting invariants RX-V4 and RX-V5
//! live in CLAUDE.md.
//!
//! v1 ships:
//! - `csharp_workspace_probe` (analysis-only, syntax_only) —
//!   expected-vs-loaded probe for `.sln` / `.slnx` workspaces.
//! - `migrate_csharp_to_filescoped_namespace` (syntax_only) —
//!   block-to-file-scoped namespace conversion.
//! - `csharp_lsp_rename` (lsp_verified) — Roslyn-LSP-backed
//!   workspace rename.
//!
//! All other kinds enumerated in the design doc currently bail with
//! `error.unimplemented_csharp_kind` so the dispatcher recognizes them
//! and surfaces the contract.

use anyhow::{Result, bail};

use crate::refactor::RefactorPlanParams;

pub(crate) mod lex;
pub mod filescoped_namespace;
pub mod find_usages;
pub mod lsp_rename;
pub mod migrate_type_usages;
pub mod organize_usings;
pub mod primary_ctor_migrate;
pub mod public_api_guard;
pub mod record_migrate;
pub mod unseal;
pub mod workspace_probe;

// Compatibility alias so submodules that referenced the old inline lex
// helpers via `super::unseal_lex::*` keep working.
pub(crate) use lex as unseal_lex;

pub use filescoped_namespace::plan_filescoped_namespace;
pub use find_usages::plan_find_csharp_usages;
pub use lsp_rename::plan_lsp_rename;
pub use migrate_type_usages::plan_migrate_type_usages;
pub use organize_usings::plan_organize_usings;
pub use primary_ctor_migrate::plan_primary_ctor_migrate;
pub use public_api_guard::plan_public_api_guard;
pub use record_migrate::plan_to_record_migrate;
pub use unseal::plan_unseal;
pub use workspace_probe::plan_workspace_probe;

/// Re-exported helpers for sibling submodules. Lives here so the LSP
/// helpers in `lsp_rename` can be called from `find_usages` without a
/// circular dependency on the public API.
pub(crate) mod lsp_rename_helpers {
    pub(crate) use super::lsp_rename::find_first_identifier_byte;
}

/// Generic v1 stub for plan kinds enumerated in the design doc but not
/// yet implemented. Returns `error.unimplemented_csharp_kind` so the
/// dispatcher acknowledges the kind and atom prompts get a clear
/// contract violation rather than a generic "unsupported plan kind"
/// surface.
pub fn plan_unimplemented(p: &RefactorPlanParams, kind: &str) -> Result<String> {
    bail!(
        "error.unimplemented_csharp_kind: `{kind}` is enumerated in design/refactor-tools/csharp/refactor-csharp-expansion.md but not yet implemented (params source={:?})",
        p.source
    );
}

/// Phase 2 stub — the plan kind requires the Roslyn sidecar architecture
/// from the design doc, which lands after Phase 1 LSP plumbing is
/// proven. Refuses with `error.csharp_sidecar_required` so callers
/// distinguish "not built yet" from "needs sidecar dispatch".
pub fn plan_sidecar_required(p: &RefactorPlanParams, kind: &str) -> Result<String> {
    bail!(
        "error.csharp_sidecar_required: `{kind}` requires the Roslyn sidecar (Phase 2; design/refactor-tools/csharp/refactor-csharp-expansion.md). Source={:?}",
        p.source
    );
}

#[allow(unused_imports)]
pub(crate) use crate::refactor::{
    FileEdit, FileMove, PlanStatus, RefactorPlan, SemanticStatus, SyntaxItem, ValidationStep,
};

/// Helper: build a fresh empty `RefactorPlan` populated for a C# kind.
/// Mirrors the shape Rust/Java helpers produce so the `validate_plan_shape`
/// checks pass with a uniform schema.
pub(crate) fn empty_plan(kind: &str, title: String, status: SemanticStatus) -> RefactorPlan {
    RefactorPlan {
        title,
        kind: kind.to_string(),
        semantic_status: status,
        dry_run: true,
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


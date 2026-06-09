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

pub mod async_dispose_convert;
pub mod awaited_query_in_loop_audit;
pub mod compile_fix_round;
pub mod filescoped_namespace;
pub mod find_usages;
pub(crate) mod lex;
pub mod lsp_move_item;
pub mod lsp_rename;
pub mod migrate_type_usages;
pub mod move_members_to_partial;
pub mod move_type_to_file;
pub mod nullable_annotation_repair;
pub mod organize_usings;
pub mod partial_class_audit;
pub mod primary_ctor_migrate;
pub mod public_api_guard;
pub mod record_migrate;
pub mod unseal;
pub mod workspace_probe;

// Compatibility alias so submodules that referenced the old inline lex
// helpers via `super::unseal_lex::*` keep working.
pub(crate) use lex as unseal_lex;

pub use async_dispose_convert::plan_async_dispose_convert;
pub use awaited_query_in_loop_audit::plan_awaited_query_audit;
pub use compile_fix_round::plan_compile_fix_round;
pub use filescoped_namespace::plan_filescoped_namespace;
pub use find_usages::plan_find_csharp_usages;
pub use lsp_move_item::plan_lsp_move_item;
pub use lsp_rename::plan_lsp_rename;
pub use migrate_type_usages::plan_migrate_type_usages;
pub use move_members_to_partial::plan_move_members_to_partial;
pub use move_type_to_file::plan_move_type_to_file;
pub use nullable_annotation_repair::plan_nullable_annotation_repair;
pub use organize_usings::plan_organize_usings;
pub use partial_class_audit::plan_partial_class_audit;
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

// Generic stubs (plan_unimplemented / plan_sidecar_required) were
// removed when every plan kind landed a real implementation. Add
// them back if a future kind ships an interim stub.

#[allow(unused_imports)]
pub(crate) use crate::{
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
    }
}

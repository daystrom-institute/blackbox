//! `rust.*` — the Rust transform cell-DSL namespace.
//!
//! Curated Rust transforms ported from the v1 `bbox_refactor` rust catalog
//! (design/refactor-tools/rust/rust-isolate-surface.md). The namespace ships
//! as a DIRECTORY, not a single file: `java_transforms.rs` is 698K and is
//! itself the next splitting candidate, so Rust starts split by family
//! (design §7). Today only the compile-fix loop (`fix.rs`) lives here; future
//! families (move/wiring/migrate/crates) land beside it.
//!
//! Porting recipe (bindings/AGENTS.md): run the v1 analysis/synthesis
//! verbatim where cleanly possible, strip the MCP/plan-apply envelope, return
//! `{changes, findings, ...}` for `edits.merge`. Transforms NEVER write; the
//! `edits.apply` choke point is the sole mutation path. Provenance is
//! host-computed lineage: tree-sitter-backed synthesis floors at
//! `syntax_only`; `rust.fixRound` is the first non-LSP producer that records
//! to the ledger, at the `compiler_suggested` tier, and only for edits whose
//! span+replacement come verbatim from a rustc/clippy `MachineApplicable`
//! `suggested_replacement`.

mod fix;

use std::sync::Arc;

use bro_code_mode::ToolNamespaceDescription;
use bro_tools::Tool;

pub use fix::{RustDescribe, RustFixRound};

/// The `rust.*` binding set. `ledger` is the session-shared provenance
/// ledger (the same instance `lsp.*` and `edits.*` hold) so
/// `rust.fixRound`'s `compiler_suggested` issuances survive to `edits.merge`.
pub fn tools(ledger: Arc<crate::bindings::ledger::ProvenanceLedger>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RustFixRound(ledger)) as Arc<dyn Tool>,
        Arc::new(RustDescribe) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
/// Compact index (§6.5 surface economics): one line per transform; depth on
/// demand via `rust.describe`.
pub fn namespace_description() -> ToolNamespaceDescription {
    ToolNamespaceDescription {
        name: "rust".to_string(),
        description: "Rust transform authorities ported from the v1 bbox-refactor rust catalog. Most transforms are tree-sitter-backed with provenance syntax_only; rust.fixRound records compiler_suggested for edits whose span+replacement come verbatim from a rustc/clippy MachineApplicable suggested_replacement (the only non-LSP producer that touches the provenance ledger). Each transform runs host-side and returns edits-algebra inputs for edits.merge - never writes. Call rust.describe({transform}) for the full contract before first use. Transforms: fixRound - classify rustc/clippy JSON diagnostics (the shape build.gate emits for cargo --message-format=json) into machine-applicable replace proposals (compiler_suggested), add-use / visibility-bump classifier proposals (syntax_only), and explicit leftovers (borrow-checker, trait-bound, type-mismatch); the loop engine of the compile-fix loop."
            .to_string(),
        declarations: r#"type RustFixChange = { span: { file: string; byte_start: number; byte_end: number; content_sha256: string }; new_text: string };
type RustFixFinding = { finding: string;[key: string]: unknown };
type RustFixLeftover = { code?: string; message: string;[key: string]: unknown };
type RustFixRoundResult = { title: string; changes: RustFixChange[]; findings: RustFixFinding[]; leftovers: RustFixLeftover[]; issuance: string | null; compiler_suggested: number; syntax_only: number; leftover_count: number; provenance: "syntax_only" | "compiler_suggested" };
declare const rust: {
  /** Classify rustc/clippy JSON diagnostics (accept the build.gate diagnostics array verbatim, or a raw rustc JSON-lines string) into edit proposals + explicit leftovers. MachineApplicable suggested_replacement edits record at compiler_suggested; add-use/visibility-bump classifier proposals floor at syntax_only. Never writes; feed changes to edits.merge. */
  fixRound(args: { diagnostics?: unknown[]; rustcJson?: string; restrictToFiles?: string[] }): Promise<RustFixRoundResult>;
  /** Full contract for one rust.* transform (params, findings vocabulary, recipe). The namespace index lists transforms one line each; call this before first use of a transform. */
  describe(args: { transform: string }): Promise<{ contract: string }>;
};"#
            .to_string(),
    }
}

//! `rust.*` — the rust transform cell bindings.
//!
//! Design home: `design/refactor-tools/rust/rust-isolate-surface.md`.
//! Trust model + porting recipe: `crates/bro-harness/src/bindings/AGENTS.md`.
//!
//! Directory layout (§7: do not write one giant file): one module per
//! concern. Today only `fix` (the compile-fix loop engine); move/wiring/
//! migrate/crates modules land as the surface grows toward the full ~15
//! transforms the design curates.
//!
//! Every transform follows the `java.*` precedent: thin adapter that NEVER
//! writes, returns `{changes, findings, leftovers}` for `edits.merge`, and
//! records host-authored changes in the session-shared provenance ledger so
//! `edits.apply` can compute `semantic_status` lineage. The namespace index
//! is a compact one-line-per-tool description; full contracts come from
//! `rust.describe` at runtime (values stay in the isolate, no prompt bloat).

mod fix;

use std::sync::Arc;

use async_trait::async_trait;
use bro_code_mode::ToolNamespaceDescription;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde_json::{Value, json};

use super::ledger::ProvenanceLedger;

/// `rust.describe` — depth-on-demand contract for one rust transform
/// (matches the `analysis.describe` / `java.describe` pattern; the namespace
/// index stays a compact one-liner).
pub struct RustDescribe;

const FIX_ROUND_CONTRACT: &str = r#"rust.fixRound — classify rustc/clippy diagnostics into reviewable edit proposals + explicit leftovers.

WHAT IT DOES
  Takes the `build.gate` diagnostics shape verbatim (the `diagnostics[]` array
  from a `cargo check --message-format=json` gate), OR raw rustc JSON-lines
  via `raw_json`, and ports the v1 `rust_compile_fix_round` classifier:

    - Verbatim `MachineApplicable` `suggested_replacement` spans become
      `{changes}` entries recorded at `compiler_suggested` provenance. These
      are compiler-authored bytes (the compiler asserts they compile) and
      keep their tier through `edits.merge` -> `edits.apply` content-digest
      recognition.
    - Classifier-synthesized proposals (add-use from E0432/E0433/E0282,
      visibility-bump from E0603/E0624/E0616, add-use from E0599) are
      synthesized at `syntax_only`. The replacement text may come from the
      compiler, but the INSERT POSITION is a planner guess — honesty about
      sourcing is what the provenance ladder preserves.
    - Borrow-checker / trait-bound / move errors (E0277, E0382, E0502,
      E0507, E0596) and uncategorized diagnostics become `leftovers`. They
      are surfaced, never blindly retried.

  Clippy rides free: it emits the same JSON shape, and `clippy::*` codes fall
  through to the leftover branch (closing gap G13 as the lint-classification
  mode of the same tool).

PARAMS
  diagnostics: BuildDiagnostic[]   The `diagnostics[]` array from build.gate.
                                       Each entry may carry `code`, `file`,
                                       `message`, and `suggestions[]`.
  raw_json?: string                 Optional raw `cargo --message-format=json`
                                       / `rustc --error-format=json` stdout;
                                       parsed and appended to `diagnostics`.
  restrict_to_files?: string[]      Optional file path substrings;
                                       diagnostics whose `file` does not
                                       match any are skipped (mirrors v1
                                       `restrict_to_files`).

RETURNS { changes, findings, leftovers, counts, issuance }
  changes[]: each is `{ span, new_text, provenance, code? }` —
    span          hash-anchored Span (from the build.gate suggestion span;
                  re-derive via code.read if absent)
    new_text      verbatim `suggested_replacement` bytes (compiler_suggested)
                  or planner-synthesized text (syntax_only)
    provenance    "compiler_suggested" | "syntax_only"
    code?         the diagnostic code that motivated the proposal
  findings[]: review notes (e.g. visibility_bump_proposed)
  leftovers[]: each is `{ message, code?, reason }` — diagnostics the cell
    must address by hand.
  counts: { changes, leftovers }

NEVER WRITES. Feed `changes` into `edits.merge`; the ledger recognition keeps
the provenance tier through to `edits.apply`. The multi-round discipline is:
  rust.<transform> -> edits.apply -> build.gate("cargo check --message-format=json")
    -> rust.fixRound -> edits.merge -> edits.apply -> (fixed point, cap ~5)
Leftovers are the manual punch list; do not retry them blindly.

IDIOM
  const gate = await build.gate({ command: "cargo check --message-format=json",
                                   anchor_spans: true });
  const round = await rust.fixRound({ diagnostics: gate.diagnostics });
  if (round.changes.length) {
    const es = await edits.begin();
    await edits.merge({ es, changes: round.changes.map(c => ({span: c.span, new_text: c.new_text})) });
    await edits.apply({ es });
  }
  // round.leftovers is the manual punch list.
"#;

#[async_trait]
impl Tool for RustDescribe {
    fn name(&self) -> &str {
        "rust.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one rust.* binding (params, result vocabulary, recipe). The namespace index lists transforms one line each; call this before first use."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "transform": { "type": "string", "description": "Transform name, e.g. \"fixRound\"." }
            },
            "required": ["transform"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "describe".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let transform = input
            .get("transform")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match transform {
            "fixRound" => ToolResult::Json(json!({ "contract": FIX_ROUND_CONTRACT })),
            other => ToolResult::Error(format!(
                "rust.describe: unknown transform `{other}` (available: fixRound)"
            )),
        }
    }
}

/// The `rust.*` binding set.
pub fn tools(ledger: Arc<ProvenanceLedger>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RustDescribe) as Arc<dyn Tool>,
        Arc::new(fix::RustFixRound(ledger)) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-DSL §5.2).
/// Compact index (§6.5 surface economics): one line per transform; depth on
/// demand via rust.describe.
pub fn namespace_description() -> ToolNamespaceDescription {
    ToolNamespaceDescription {
        name: "rust".to_string(),
        description: "Rust transform bindings ported from the v1 bbox-refactor rust catalog (design/refactor-tools/rust/rust-isolate-surface.md). Each transform NEVER writes: it returns {changes, findings, leftovers} for edits.merge and records host-authored changes in the provenance ledger so edits.apply computes semantic_status lineage. Call rust.describe({transform}) for the full contract. Transforms: fixRound - classify rustc/clippy build.gate diagnostics into compiler_suggested edits (verbatim MachineApplicable suggestions) + syntax_only synthesized proposals (add-use, visibility-bump) + explicit leftovers (borrow-checker, trait-bound); the compile-fix loop engine. Clippy diagnostics classify the same way (same JSON shape)."
            .to_string(),
        declarations: r#"type RustChangeProposal = { span: Span; new_text: string; provenance: "compiler_suggested" | "syntax_only"; code?: string };
type RustLeftover = { message: string; code?: string; reason: string };
type RustFixRoundResult = { changes: RustChangeProposal[]; findings: ({ finding: string } & Record<string, unknown>)[]; leftovers: RustLeftover[]; counts: { changes: number; leftovers: number }; issuance: string };
declare const rust: {
  /** Full contract (params, result vocabulary, recipe) for one rust transform. Call before first use. */
  describe(args: { transform: string }): Promise<{ contract: string }>;
  /** Classify rustc/clippy build.gate diagnostics into edit proposals + leftovers. Verbatim MachineApplicable suggestions become compiler_suggested changes; add-use/visibility-bump proposals are syntax_only; borrow-checker/trait-bound errors are leftovers. NEVER writes: feed {changes} into edits.merge. */
  fixRound(args: { diagnostics: Record<string, unknown>[]; raw_json?: string; restrict_to_files?: string[]; restrictToFiles?: string[] }): Promise<RustFixRoundResult>;
};"#
            .to_string(),
    }
}

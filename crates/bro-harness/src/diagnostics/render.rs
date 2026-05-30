//! Wave-2 PRESENTATION (Drone 2 owns this file).
//!
//! Responsibilities:
//! 1. [`classify`] — map a raw `lsp_types::Diagnostic` to the window-0
//!    [`Enveloped`] form. MVP: Rust error tier — `class = Error`,
//!    `tier = Instant`, `confidence = Confirmed`. (Lints/candidate/deferred are
//!    later waves; shape the code so adding them is small.)
//! 2. [`build_rider`] — render the block appended to a tool result from the
//!    spine's `&[DiffResult]`. Surface only `DiffResult.new`; lead with a
//!    concise, SCOPE-HONEST summary (a clean result is a scoped claim, e.g.
//!    "no new errors [pkg=…, features=…]", never bare "all good"). Mention
//!    `fixed`/`carried` counts where useful. Return `None` when there is
//!    nothing new to report (no rider appended).
//!
//! Keep the rider compact and high-signal — it rides EVERY edit's tool result,
//! so verbosity is a tax on the agent's context. Errors first; include file +
//! line + the rust-analyzer message; group by file.

#![allow(unused)]

use super::{Confidence, DiagClass, DiffResult, Enveloped, Scope, Tier};
use lsp_types::Diagnostic;

/// Classify a raw LSP diagnostic into the window-0 envelope. MVP: Rust error tier.
pub fn classify(diag: &Diagnostic, scope: &Scope) -> Enveloped {
    let _ = (diag, scope);
    todo!("Drone 2: classify a raw diagnostic into the window-0 envelope")
}

/// Build the rider block appended to a tool result, or `None` if nothing is new.
pub fn build_rider(diffs: &[DiffResult]) -> Option<String> {
    let _ = diffs;
    todo!("Drone 2: render the window-0 rider from the diff results")
}

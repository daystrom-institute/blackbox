//! Wave-2 SPINE (Drone 1 owns this file + the `agent_loop` seam wiring).
//!
//! Responsibilities:
//! 1. After a mutating tool dispatch, drain the `bro-tools` `EditSink` and, for
//!    each edited file, run `bro-lsp` (open / `didChange` / pull) to get the
//!    fresh `Vec<lsp_types::Diagnostic>`.
//! 2. Diff the fresh pass against the file's baseline in
//!    [`crate::lsp_baselines::LspBaselines`] using a STABLE, line-number-
//!    independent identity (e.g. code + message + normalized span/symbol), so a
//!    diagnostic that merely shifted lines is NOT reported as new.
//! 3. Update the baseline with the fresh pass after diffing.
//! 4. Return one [`DiffResult`] per edited file.
//! 5. Wire this into the edit loop at the post-dispatch seam in
//!    `agent_loop.rs` (~487-530): if the `EditSink` is non-empty, drain →
//!    `check_edits` → `render::build_rider` → append the rider to the tool
//!    result `content` (same append shape as `bound.rs`).
//!
//! Design notes (yours to finalize — you own both ends of this function):
//! - The `bro_lsp::SessionPool` should be LOOP-LIVED (warm sessions reused
//!   across edits), not constructed per call. Thread it through the loop state;
//!   the signature below is a starting point you may refine, as long as you
//!   still return `Vec<DiffResult>` for `render::build_rider`.
//! - Honor drop-stale: pull diagnostics for the version you just applied; a
//!   `Superseded` result must not be surfaced.
//! - MVP scope: Rust + rust-analyzer, error tier. Detect language from the file
//!   extension; skip non-Rust files for now.

#![allow(unused)]

use super::DiffResult;
use crate::lsp_baselines::LspBaselines;
use bro_tools::edits::EditEvent;
use std::path::Path;

/// Run window-0 diagnostics for a batch of edits, diff against baselines, and
/// return per-file new/changed findings. Updates `baselines` in place.
pub async fn check_edits(
    edits: &[EditEvent],
    baselines: &mut LspBaselines,
    pool: &bro_lsp::SessionPool,
    root: &Path,
) -> anyhow::Result<Vec<DiffResult>> {
    let _ = (edits, baselines, pool, root);
    todo!("Drone 1: implement the window-0 spine (run bro-lsp, diff vs baseline, update baseline)")
}

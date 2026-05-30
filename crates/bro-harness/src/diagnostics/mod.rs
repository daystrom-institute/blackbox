//! Window-0 diagnostics pipeline (wave 2).
//!
//! Flow: a mutating tool dispatch records edits into the `bro-tools` `EditSink`;
//! the edit loop drains them, runs `bro-lsp` against each edited file, diffs the
//! fresh diagnostics against the per-file baseline in [`crate::lsp_baselines`],
//! classifies what is new, and riders a concise block onto the tool result.
//!
//! Ownership split (wave-2 drones build against the types in THIS file):
//! - [`engine`] — the SPINE: drain edits → run bro-lsp → diff vs baseline
//!   (stable, line-number-independent identity) → update baseline, plus the
//!   `agent_loop` seam wiring.
//! - [`render`] — PRESENTATION: classify raw diagnostics into the window-0
//!   envelope and render the rider string.
//!
//! The fixed cross-drone contract is [`DiffResult`] (produced by the spine,
//! consumed by presentation) and [`render::build_rider`]. The envelope
//! vocabulary below is consumed by `render`.
//!
//! Scaffold note: types here are unused until the drones wire the pipeline;
//! the `allow(dead_code)` is removed once the spine/presentation are live.
#![allow(dead_code)]

use lsp_types::Diagnostic;

pub mod engine;
pub mod render;

/// New/changed diagnostics for one edited file, relative to its baseline.
///
/// The spine produces one of these per edited file. `new` holds RAW diagnostics
/// (presentation owns classification); `fixed`/`carried` are counts the rider
/// can mention so a clean result reads as a *scoped* claim, not "all good".
#[derive(Debug, Clone)]
pub struct DiffResult {
    /// Worktree-relative path of the edited file.
    pub file: String,
    /// Diagnostics newly introduced (or changed) vs the file's baseline,
    /// matched by a structural identity (NOT line number) so a warning that
    /// merely shifted lines is not reported as new.
    pub new: Vec<Diagnostic>,
    /// Count of baseline diagnostics that disappeared (fixed by this edit).
    pub fixed: usize,
    /// Count of pre-existing diagnostics carried unchanged (not surfaced).
    pub carried: usize,
}

// ---- window-0 envelope vocabulary (consumed by `render`) ----

/// Hard error (won't compile) vs lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagClass {
    Error,
    Lint,
}

/// Which analyzer tier produced the verdict. MVP surfaces only `Instant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// In-memory language-server analysis (errors/types/resolution).
    Instant,
    /// Flycheck / `cargo check` tier (lints).
    Check,
    /// Workspace/all-config truth tier (orchestrator-owned; not harness-run).
    Truth,
}

/// How sure we are at the proven scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Hard error, or a lint sound at the proven scope.
    Confirmed,
    /// Config-fragile (e.g. valid only under the checked feature set).
    Candidate,
    /// Needs a truth-tier run to confirm.
    Deferred,
}

/// The exact scope a verdict was proven under. MVP: package + feature set.
#[derive(Debug, Clone)]
pub struct Scope {
    pub pkg: String,
    pub features: String,
}

/// A diagnostic plus its window-0 envelope. Produced by [`render::classify`].
#[derive(Debug, Clone)]
pub struct Enveloped {
    pub diagnostic: Diagnostic,
    pub class: DiagClass,
    pub tier: Tier,
    pub confidence: Confidence,
    pub scope: Scope,
}

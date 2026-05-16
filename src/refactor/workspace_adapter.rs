// Phase 1 scaffold — adapter trait + types + resolver stub. The
// runner does not yet call into the adapter (resolve always returns
// None). Phase 2 wires the call sites; suppress dead-code noise
// until then so the scaffold doesn't pollute the lint surface.
#![allow(dead_code)]

//! Language-scoped workspace transaction adapter for `bbox_refactor_run`.
//!
//! Designed for the C# Roslyn sidecar (Phase 2) but written
//! language-agnostically so other backends (rust-analyzer DocumentMap,
//! jdtls workspace) can plug in if they ever need workspace-snapshot
//! coherence with on-disk plan steps.
//!
//! The Rust and Java tracks today work entirely against disk — every
//! plan step's edits hit the file system and the next plan step reads
//! the new content. They have no in-memory workspace snapshot to keep
//! coherent, so [`resolve_workspace_adapter`] returns `None` for them
//! and the runner short-circuits every adapter call.
//!
//! The C# track will return a `RoslynWorkspaceTransactionAdapter` once
//! the sidecar lands (Phase 2). At that point the runner calls each
//! method at the documented sites in
//! `design/refactor-tools/csharp/refactor-csharp-expansion.md`:
//!
//! - `begin` immediately after the adapter pre-scan succeeds, before
//!   the steps loop.
//! - `apply_plan_step` after every successful `RefactorRunStep::Plan`
//!   write to disk.
//! - `apply_command_touches` after every `RefactorRunStep::Command`
//!   with a non-empty `touches` array whose outcome was **not** a
//!   rollback (includes `optional` / `continue_for_repair` failures,
//!   marked via `AppliedCommandTouches.succeeded`).
//! - `commit` on the success exit path.
//! - `rollback` on **every** rollback path. Disk rollback must run
//!   first; the adapter rollback follows. (Rationale: disk is the
//!   canonical state; the sidecar can be re-loaded if it drifted, but
//!   on-disk content cannot be reconstructed from an in-memory
//!   workspace alone.)
//!
//! Adapter selection is done by walking the step list before calling
//! `begin`: every plan-kind prefix must resolve to the same adapter
//! (or to `None`). Mixed-language step lists refuse with
//! `error.mixed_workspace_adapters`. Command-only step lists skip
//! adapter resolution entirely.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::{FileEdit, FileMove, RefactorRunStep};

/// Full diff that an applied plan step produces, mirrored to the
/// language-scoped workspace snapshot held by the adapter.
///
/// `edits` are in-file text replacements (the runner has already
/// written them to disk before calling `apply_plan_step`).
/// `file_moves` are renames / relocations from
/// `RefactorPlan.file_moves`. `created` and `deleted` are the
/// runner-derived sets for files that came into or out of existence
/// as a side effect of the edits — the C# sidecar needs them so
/// `Solution.AddDocument` / `RemoveDocument` stays consistent with
/// disk.
pub struct AppliedPlanDelta<'a> {
    pub edits: &'a [FileEdit],
    pub file_moves: &'a [FileMove],
    pub created: &'a [PathBuf],
    pub deleted: &'a [PathBuf],
}

/// Mirror set for a `RefactorRunStep::Command` step whose touches
/// list mutated on disk. `succeeded` is `false` when the command
/// failed but the runner did not roll back (`optional` or
/// `continue_for_repair`); the sidecar still re-reads the touched
/// files because they may have been partially mutated before the
/// failure.
pub struct AppliedCommandTouches<'a> {
    pub touches: &'a [PathBuf],
    pub succeeded: bool,
}

/// Trait for language-scoped workspace snapshot adapters. The trait
/// is intentionally minimal — the C# sidecar implementation does the
/// heavy lifting in its protocol, this trait is the runner-side
/// hook.
///
/// Implementations are pooled by `(language, workspace_root)` upstream
/// of the resolver below.
pub trait WorkspaceTransactionAdapter: Send + Sync {
    /// Open a transaction scoped to one `bbox_refactor_run` invocation.
    /// Cheap — the C# adapter snapshots the immutable `Solution`
    /// reference. Failing here MUST be benign: the runner continues
    /// with disk-only semantics if `begin` errors (the adapter logs
    /// the error and `apply_*` calls become no-ops for this run).
    fn begin(&self, run_id: &str, project: &Path) -> Result<()>;

    /// Mirror an applied plan-step delta into the snapshot. See
    /// [`AppliedPlanDelta`].
    fn apply_plan_step(&self, run_id: &str, delta: &AppliedPlanDelta<'_>) -> Result<()>;

    /// Mirror command-touch mutations into the snapshot. Called on
    /// any non-rollback command outcome — including failed-but-continued
    /// commands; see [`AppliedCommandTouches::succeeded`].
    fn apply_command_touches(
        &self,
        run_id: &str,
        touches: &AppliedCommandTouches<'_>,
    ) -> Result<()>;

    /// Promote the current snapshot to the new baseline. Called once
    /// on the success exit path.
    fn commit(&self, run_id: &str) -> Result<()>;

    /// Restore the snapshot to its pre-`begin` state. Called on every
    /// runner rollback path. The runner has already restored disk
    /// before invoking this method — disk is canonical.
    fn rollback(&self, run_id: &str) -> Result<()>;
}

/// Resolve the workspace adapter for a compound run, based on the
/// plan-kind prefixes that appear in the step list.
///
/// v1 always returns `None` — the C# track returns a real adapter
/// once the Roslyn sidecar lands (Phase 2). Returning `None` here
/// short-circuits every adapter call in the runner — no behavior
/// change for Rust / Java / generic kinds.
///
/// When Phase 2 ships, this function:
/// 1. Walks `steps` and collects every `RefactorRunStep::Plan`'s
///    kind prefix (e.g. `csharp_*`, `rust_*`, `java_*`).
/// 2. If all prefixes map to the same language adapter, returns it.
/// 3. If multiple incompatible adapters are required, returns
///    `Err(MixedWorkspaceAdapters { kinds })` so the runner refuses.
/// 4. If the step list is command-only or has no plan steps, returns
///    `Ok(None)` (no adapter to mirror into).
pub fn resolve_workspace_adapter(
    steps: &[RefactorRunStep],
    project: &Path,
) -> Result<Option<Arc<dyn WorkspaceTransactionAdapter>>> {
    // Delegate to the C# sidecar resolver. When the steps are
    // entirely non-csharp_*, it returns Ok(None) and the runner
    // short-circuits. When mixed, it raises
    // error.mixed_workspace_adapters. When the sidecar binary is
    // unavailable, it returns Ok(None) and the runner uses
    // disk-only semantics — Phase 1 plan kinds still work.
    super::csharp_sidecar::resolve_csharp_adapter(steps, project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_none_in_phase_1() {
        let steps: Vec<RefactorRunStep> = Vec::new();
        let project = Path::new("/tmp/anywhere");
        let result = resolve_workspace_adapter(&steps, project).unwrap();
        assert!(
            result.is_none(),
            "Phase 1 always returns None — no adapter implementations registered yet"
        );
    }

    #[test]
    fn applied_plan_delta_holds_borrowed_slices() {
        // Sanity check that the lifetime story compiles cleanly —
        // adapter implementations consume borrowed slices, not owned
        // copies.
        let edits: Vec<FileEdit> = Vec::new();
        let moves: Vec<FileMove> = Vec::new();
        let created: Vec<PathBuf> = Vec::new();
        let deleted: Vec<PathBuf> = Vec::new();
        let delta = AppliedPlanDelta {
            edits: &edits,
            file_moves: &moves,
            created: &created,
            deleted: &deleted,
        };
        assert_eq!(delta.edits.len(), 0);
        assert_eq!(delta.file_moves.len(), 0);
    }

    #[test]
    fn applied_command_touches_carries_succeeded_flag() {
        let touches: Vec<PathBuf> = vec![PathBuf::from("/tmp/x.cs")];
        let mirror = AppliedCommandTouches {
            touches: &touches,
            succeeded: false,
        };
        assert_eq!(mirror.touches.len(), 1);
        assert!(!mirror.succeeded);
    }
}

//! Shared project write-scope resolution for store tool adapters.
//!
//! Stores that key durable state by project path (knowledge, gaps, and —
//! progressively — pins/notes/whiteboards/roadmap) must agree on what a
//! caller-supplied `project` value means when the caller works inside a
//! worktree: the durable scope is the registered BASE project, while
//! repo-owned committed files belong in the WORKTREE checkout so they travel
//! with the agent's branch. Centralizing the resolution here keeps every
//! store's interpretation identical (gap-de82a74d: bbox_learn and bbox_render
//! disagreeing on scope made worktree-written entries unrenderable).

use crate::server::BlackboxServer;

impl BlackboxServer {
    /// Resolve a raw `project` path/id to `(durable_scope, write_dir)`.
    ///
    /// - Recognized worktrees (managed fleet worktrees AND in-tree linked
    ///   worktrees like `.claude/worktrees/<name>`) key to the registered
    ///   base; `write_dir = Some(worktree)` redirects repo-owned committed
    ///   files into the worktree checkout.
    /// - Other registered projects resolve through the registry to their
    ///   canonical path (`write_dir = None`).
    /// - Unregistered paths fall back to filesystem canonicalization;
    ///   non-path values (registry misses) pass through untouched.
    // Blocking fs (canonicalize/git probes): call from run_blocking /
    // spawn_blocking closures only, like the store mutations it scopes.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn resolve_project_write_scope(&self, raw: &str) -> (String, Option<String>) {
        if let Some((base, worktree)) =
            crate::projects::fleet_worktree_scope_and_dir(raw, &self.state.projects.read().list())
        {
            return (base, Some(worktree));
        }
        if let Ok(Some(record)) = self.state.projects.read().resolve(raw) {
            return (record.canonical_path, None);
        }
        let project = std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| raw.to_string());
        (project, None)
    }

    /// Filter-side companion to [`Self::resolve_project_write_scope`]: map a
    /// project FILTER value to its registered base only when it is a
    /// recognized worktree path (`None` otherwise — caller keeps the raw
    /// value). Substring filters and other non-path values pass through
    /// untouched, preserving each store's existing match semantics.
    pub(crate) fn rescope_project_filter_value(&self, raw: &str) -> Option<String> {
        crate::projects::fleet_worktree_scope_and_dir(raw, &self.state.projects.read().list())
            .map(|(base, _worktree)| base)
    }
}

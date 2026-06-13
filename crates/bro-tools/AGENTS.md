# bro-tools — harness tool surface + fleet worktree closeout driver

## Workspace search output shape

- `content_search` is exploratory by default, not an exhaustive dump. Keep the
  default result cap and byte cap small enough for an agent turn, and append
  refinement hints when truncating. Integrity comes from honest truncation
  metadata plus `max_results` opt-in, not from silently returning every matching
  line (gap-0c902d6d).

## Closeout phased driver (`fleet_worktree.rs`)

- The driver does the MECHANICAL fold: preflight → stage/commit → ff-base →
  rebase → ff-merge → (hooks) → push → remove, returning structured
  per-phase results. Judgment never lives here — the cockpit requests it
  from the agent (commit message, conflict resolution) or defers it to the
  operator (anything touching their base-branch history).
- **Diverged base is a deferral, not a failure.** When the local target and
  `origin/<target>` each carry commits the other lacks, the local fold still
  completes; push AND worktree removal are deferred (marker phase with an
  operator-facing message: reconcile, then `/closeout adopt` finishes). The
  worktree must survive deferral — adopt is the finish path.
- **The push-reject recovery's `reset --hard origin/<target>` is safe only
  under the ff_base invariant** (local was synced to origin when the fold
  started, so the only local-only commits are the fold's own ff, which the
  worktree branch preserves). Widening that recovery's reach — or letting a
  diverged fold get anywhere near it — erases operator commits. If you touch
  the phase ordering, re-derive this invariant first.
- Worktree-local rebase conflicts are classified (`RebaseConflict`,
  `repo_cwd == worktree`) so the cockpit can hand them to the owning agent
  and rerun as adopt. Keep error classes precise; the cockpit routes on them.
- Phase tests use real git fixtures (temp repo + bare origin + linked
  worktree). When adding behavior, pin it with one of those — closeout bugs
  are repo-state bugs and unit assertions on JSON shapes don't catch them.
- Worktree removal auto-reclaims per-worktree build dirs; branch deletion is
  best-effort and must be reported, never silently claimed.

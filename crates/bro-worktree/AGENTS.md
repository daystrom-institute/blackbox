# Fleet worktree closeout

This crate owns the mechanical closeout phases used by the daemon. It contains
no model tools or provider runtime. Keep preflight, staging, fast-forward, rebase,
merge, hooks, push and removal outcomes distinct so callers can recover safely.

A diverged base defers push and worktree removal. Push-rejection recovery may
reset the base only after the fast-forward precondition proved that the worktree
branch preserves all local-only commits. Never remove a dirty worktree or force
branch deletion during a non-discard closeout.

Tests use temporary repositories and bare remotes. Keep environment changes
serialized and scoped to fixture paths.

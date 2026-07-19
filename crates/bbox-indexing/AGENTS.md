# bbox-indexing — project registry + THE project resolver

## resolve_project_context is the single entry point

- Selector order: exact project_id / canonical path → unique registered
  alias → filesystem path, gated by `ResolveIntent`. Any new tool param that
  accepts project-like input resolves through this — do not grow bespoke
  resolution chains. (The 2026-06 taxonomy consolidation collapsed three
  independent ones; the design is
  design/corpus/agentic-corpus/project-taxonomy-standardization.md.)
- **The Read/Write intent asymmetry is deliberate and load-bearing.** Read
  uses the broad retrieval gate: descendants and ANY worktree of a
  registered repo alias to the base — scoping a query is harmless. Write
  uses the conservative managed gate (`resolve_managed_fleet_worktree`):
  only fleet/agent-dispatch and in-tree linked worktrees alias; everything
  else returns None so write-side callers keep their fail-closed fallbacks.
  Where gap files, threads, and rendered files LAND depends on this.
  Collapsing the gates writes repo-owned state into arbitrary user
  worktrees.
- Known quirk, deliberately NOT codified in the resolver: legacy write
  chains let a plain subdirectory of a registered root fall through to
  canonicalize-pass-through (keying state under the subdir). The resolver
  returns None for that case under Write — the fallback decision stays
  explicit at call sites.

## Aliases fail closed at every layer

- Declared in the repo's committed `.bbox/config.toml` `[project] aliases`;
  the registry materializes them at registration (conflict bails the call —
  fix the config and re-register to converge) and at daemon open (conflict
  is skipped + warned; first-claim-wins is deterministic via canonical_path
  sort). An alias claimed by more than one record — possible only via a
  hand-edited store — resolves to nothing.
- Sync REPLACES the record's alias set with the declared set: the committed
  config is authoritative. Host-local operator aliases would need a separate
  field; don't overload this one.
- `managed_fleet_worktree_project`'s `:fleet-worktree` pseudo-id is a compat
  shim that ProjectContext should eventually subsume — don't add consumers.

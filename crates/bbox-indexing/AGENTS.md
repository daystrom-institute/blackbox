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
- Pool lanes are full clones rather than linked worktrees. Their exact
  `.git/blackbox-managed-checkout` marker plus a matching durable `repo_id`
  admits them through the conservative write gate. Never weaken this to
  origin URL, path shape, or arbitrary-clone matching.

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

## Durable project catalog transactions are paired

- `projects.json` and `project-attachments.json` are one logical value. Every
  strict mutation installs and validates complete post-images at one matching
  nonzero epoch. No caller may write one participant directly.
- Lock order is the process-lifetime migration lock, then the canonical
  `projects.json.lock`, then code-owned auxiliary store locks in deterministic
  path order. The lifetime lock prevents bridge/offline overlap; the short
  locks serialize bridge writes, strict reads, recovery, and transactions.
  Code-source and accepted-publication writers must share the same anchor
  store locks used by the migration participant registry.
- Recovery is journal-driven and fail-closed. It may complete the whole new
  participant set or restore the whole old set only when every required byte
  image is installed or available in a verified code-owned artifact. Never
  synthesize missing bytes, accept a mixed set, or recover only the catalog
  subset of a migration journal.
- Strict store reads reject symlinks, non-regular files, oversized input,
  legacy v1 bytes, half-pairs, mismatched epochs, and invalid cross-store
  references. A fresh v2 origin forbids a migration marker; a migrated origin
  requires the committed marker for its exact transaction.
- Migration checkout-ID actions share the `.bbox/local` directory lock with
  `ensure_checkout_id`. The owner holds a component-no-follow directory
  descriptor and performs marker and gitignore I/O relative to that exact
  inode. Missing or empty markers may be atomically filled, and any different
  or unsafe marker refuses the migration. A successfully installed ID is
  monotonic and is not rolled back with catalog participants.

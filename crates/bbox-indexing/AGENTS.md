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
- Repo-history generations have one canonical builder and exactly three
  caller classes: pre-replacement/Phase-6 materialization, live checkout
  refresh, and verified typed-producer refresh. A producer activation may
  prepare the builder's exact future id for its journal, but cannot encode or
  publish a generation through another path. Publication replaces the whole
  `(repo_id, doc_type=commit)` lane before re-emitting, because a complete
  force-pushed source can remove commits that entity-only upsert would strand;
  `repo_id` alone is forbidden because code chunks share it.
- Typed history publication requires exact equality between the projects with
  materialized edge rows and the projects with snapshot ids. The writer actor
  must not commit edges for a project without staging and finalizing that same
  project's durable snapshot receipt.

## Graph word lanes replace whole lanes

- One graph's word-index documents are keyed `(project_id, graph_id, plane)`.
  Activation replaces the whole lane (or purges it) through the writer actor;
  per-document upserts are forbidden because a generation flip may remove
  vertices. The generation stamp is the no-op key: a re-activation carrying
  the same stamp writes nothing.
- A graph whose policy disables text retrieval, and a graph that left the
  accepted view, must have ZERO documents in the index. Purging the lane is
  the mechanism; filtering at query time is not a substitute.
- Property text reaches the index only through schema annotations
  (`index: word` into the code-tokenized lane, `index: text` into the prose
  body) under the graph's index policy. Unannotated property values and
  schema-as-data (meta) vertices are never indexed.
- Every graph vertex document carries `project_id` as an exact term (Q6).
  Query-side project scoping filters on that field; it never parses the
  entity ref or consults the catalog inside the filter.
- Full reindex passes preserve graph lanes like provisional knowledge: there
  is no durable store the pass walks for them. The schema-migration rebuild
  starts empty and lanes re-activate at the next accepted view install,
  mirroring the in-memory view catalog's own lifecycle.
- Lane activation is install-driven, never reindex-driven: a newly published
  generation reaches the word index only when a published view INSTALLS
  (accept-advance of the pushed commit, boot reconcile, or a capture with no
  published view installed). Between the push and that install there is a
  real convergence window, minutes wide, during which the graph validates and
  inspects fine but its vertices do not surface in hybrid search. Diagnose
  "published but not searchable" with the describe retrieval block
  (`indexed_generation` vs `accepted_generation`, `indexed_vertex_count`)
  before suspecting the indexer; an incremental reindex proves nothing here
  and must not be filed as the missing trigger (gap-7bc434bf was exactly that
  misdiagnosis).

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
- The legacy path LEDGER is wider than the fourteen-owner set. Every binding's
  `source_store` classifies as either a durable owner row (stampable,
  verifiable) or a typed non-owner source: an attachment RELOCATION mints one
  whenever a checkout moves, and it names an attachment, not a row any owner
  holds. Non-owner sources are retained and hashed exactly as read (they are
  part of the predecessor a plan is bound to) and excluded from the owner
  population everywhere: no stamp, no read-back, and counted as neither
  mappable nor quarantined nor unscoped. Plan and verify must classify through
  the same function and skip in the same place, or every count verify compares
  against the journal is off by the non-owner bindings. Treating an
  unrecognized token as a defect is still right; treating a RECOGNIZED
  non-owner one that way refused the backfill of any host that had ever
  relocated a checkout.
- Migration checkout-ID actions share the `.bbox/local` directory lock with
  `ensure_checkout_id`. The owner holds a component-no-follow directory
  descriptor and performs marker and gitignore I/O relative to that exact
  inode. Missing or empty markers may be atomically filled, and any different
  or unsafe marker refuses the migration. A successfully installed ID is
  monotonic and is not rolled back with catalog participants.
- Canonical checkout roots have two sources, and only one of them is a
  directory listing. A rehearsal enumerates its replica tree; a CONFIGURED
  layout has no replica tree, so its roots are the canonical paths the v1
  store registers, decoded through the same bounded no-follow reader that
  observes those records. Registered paths that are absent are skipped, not
  refused, and stay missing-path records. Discovery is unlocked by necessity
  (installed verification already holds the mutation lock when it asks) and
  is therefore advisory: the locked capture re-reads the store, and a root
  set that no longer matches it fails closed there.

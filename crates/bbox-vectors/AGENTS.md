# bbox-vectors — WAL-backed vector store + in-house HNSW

Invariants from the gap-2eabd96d incident (17% of the prod partition silently
unreachable). Verify shapes against code; do not violate these without
explicit design.

## Graph connectivity IS recall

- Search follows FORWARD edges from the entry point and never uses tombstones
  as waypoints. A node with zero inbound active edges is unfindable at any
  `ef` — and `zero_in_degree_nodes` has equaled `disconnected_nodes` on every
  real measurement. Watch the zero-in metric first; out-degree stats cannot
  see orphaning.
- **Never shrink a saturated neighbor list by pure distance.**
  `add_reverse_edge` must keep the `select_neighbors` diversity heuristic
  (the HNSW paper's shrink-connections; its distance backfill doubles as
  keep-pruned-connections). Distance-truncate mass-orphans near-duplicate
  clusters larger than m0: every saturated list evicts the same global
  losers, in-degree hits zero. Measured cost before the fix: 17% of prod
  disconnected, 35% self-recall loss in the repro.
- Transcript corpora are near-duplicate-heavy by construction (repeated tool
  outputs, session boilerplate). A connectivity test whose clusters are
  smaller than m0 (= 2·m, so 64 at defaults) is blind —
  `donor_recall_parity_1000` (~50/cluster) passed throughout the incident.
  `large_near_duplicate_clusters_stay_connected_under_push_order` exists for
  exactly this; keep cluster size ≫ m0 in anything new, and insert in
  cluster-consecutive order (prod's incremental arrival order is the worst
  case).
- ~1% residual disconnection on real corpora is exact-duplicate degeneracy:
  identical chunks under different entity ids tie all pairwise distances at
  zero, so no selection heuristic can discriminate. Mostly benign (the twin
  answers the query). Eliminating it needs a post-build orphan-reconnect
  pass — don't chase it with pruning tweaks.
- `self_recall_probe` is the recall diagnostic (O(sample × search)). It must
  never run on the `metrics()` path — metrics are computed per search
  response. Operationally it is exposed via `bbox_embed_status`
  `recall_probe_route`, through `VectorStore::self_recall_probe` which uses
  `try_read` — a probe during a rebuild errors "busy" instead of hanging.

## Connectivity guard (gap-1168b0bd)

- Connectivity repair has a dedicated daily service loop, with the first pass
  delayed a full day after startup. It scans diagnostics with a two-second
  deadline and attempts at most one repair per pass. Ordinary tombstone/WAL
  cleanup keeps its five-minute cadence. Both share a per-store maintenance
  try-lock with manual rebuilds; busy work is deferred.
- Rebuilds copy active vectors under a read lock, then release it before HNSW
  construction. Publication checks partition identity, WAL/slab counters and
  rebuild generation. Concurrent ingest, removal or replacement defers a stale
  result. WAL and derived-file publication still holds a write lock and can
  delay readers; do not describe this as a constant-time or lock-free swap.
- Thresholds: `COMPACT_CONNECTIVITY_RATIO` 0.05 / `NOTIFY_CONNECTIVITY_RATIO`
  0.02, calibrated from the gap-2eabd96d incident (16.7% at detection,
  ~1.4% post-rebuild residual, ≤0.3% healthy). `connectivity_breach`
  applies the `MIN_CONNECTIVITY_GUARD_NODES` (1,000) floor — tiny-graph
  ratios are noise; callers must not bypass it.
- **Surfaces that must never stall behind a rebuild read
  `metrics_nonblocking()`** (try_read per partition, busy partitions
  omitted) — the inbox attention layer does. Plain `metrics()` blocks
  behind a write-lock hold for the rebuild's full duration.

## Rebuild and persistence semantics

- The partition snapshot persists the graph as well as vectors, so restarting
  preserves graph defects. Repair uses `VectorStore::rebuild` or the daily
  connectivity maintenance loop; it does not depend on a workflow runtime.
- A rebuild can take minutes and needs memory for the active-vector snapshot
  plus the replacement graph. Diagnostic deadlines bound diagnostics only.
  The expensive graph construction holds no partition lock; final persistence
  still needs a maintenance window when even a shorter reader delay is unsafe.
- Incremental `push` and bulk `build` both funnel through
  `insert_internal` → `add_reverse_edge`: a fix (or a regression) in that
  path applies to both. Levels are deterministic from the id hash, so
  rebuilds are reproducible.

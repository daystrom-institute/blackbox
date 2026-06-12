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
  response.

## Rebuild and persistence semantics

- The partition snapshot persists the GRAPH, not just the vectors: a daemon
  restart restores a broken graph verbatim. Repair is
  `vectors::rebuild(route)` (compact → bulk `HnswIndex::build`), reachable
  as the `rebuild_hnsw` workflow op.
- `compact()` holds the partition write lock for the entire rebuild —
  399k × 1024d took ~25 minutes and starves the vector lane meanwhile.
  Routine compaction belongs in `embed-compaction-arc` (quiesce → rebuild →
  swap); a bare rebuild op is a maintenance-window move.
- Incremental `push` and bulk `build` both funnel through
  `insert_internal` → `add_reverse_edge`: a fix (or a regression) in that
  path applies to both. Levels are deterministic from the id hash, so
  rebuilds are reproducible.

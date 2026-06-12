# bbox-mcp-tools — graph retrieval pipeline (hybrid search, discover, paths)

- Hybrid pipeline order: BM25 field boosts → RRF fusion (k=60, vector weight
  0.6 default) → per-hit rerank (type × temporal, capped) → file
  aggregation / per-file dedup / modal diversification. Every constant in
  that chain is either derived or empirically swept; change them with the
  metrics harness (bbox-corpus-core search/metrics.rs), not by feel.
  `rerank_cap` on HybridSearchParams is the operator probe for re-sweeping —
  it is not a caller-facing ranking knob.
- `resolve_project_filter`: registered selectors (alias, id, any path inside
  a registered checkout or worktree) MUST resolve through the shared
  Read-intent resolver to the BASE project_id before the deterministic
  path-hash fallback. A worktree path that reaches the hash derives a
  foreign id and silently returns zero results — the ordering is
  load-bearing, not stylistic.
- The whole pipeline runs synchronously on the blocking pool; query
  embedding blocks in place and is memoized by bbox-embed's process-wide
  query cache (keyed provider+model+query — a repeat query across vector
  routes embeds once). Don't introduce async stages mid-pipeline, and don't
  re-add per-call embedding dedup maps — the cache is the dedup.
- `discover_seed` reuses `hybrid_search_typed` verbatim and differs only in
  post-processing (notable edges). Ranking changes land in one place and
  affect both; do not fork the ranking for one surface.

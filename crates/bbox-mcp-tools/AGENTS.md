# bbox-mcp-tools — graph retrieval pipeline (hybrid search, discover, paths)

- Hybrid pipeline order: BM25 field boosts → RRF fusion (k=60, vector weight
  0.6 default) → model rerank (DEFAULT since the measured 2026-07-11
  eval win: fused top-k to the [embed.rerank] cross-encoder, default
  rerank-2.5-lite; scores land in a strictly higher band than the unsent
  tail; API failure degrades to the heuristic path with
  degraded.rerank_unavailable; rerank="heuristic"/"none" are the per-call
  opt-outs) → per-hit heuristic rerank (type × temporal, capped) → file
  aggregation / per-file dedup / modal diversification. Every constant in that chain is either derived
  or empirically swept; change them with the metrics harness
  (bbox-corpus-core search/metrics.rs), not by feel. `rerank_cap` on
  HybridSearchParams is the operator probe for re-sweeping — it is not a
  caller-facing ranking knob.
- `resolve_project_filter`: registered selectors (alias, id, any path inside
  a registered checkout or worktree) MUST resolve through the shared
  Read-intent resolver to the BASE project_id before the deterministic
  path-hash fallback. A worktree path that reaches the hash derives a
  foreign id and silently returns zero results — the ordering is
  load-bearing, not stylistic.
- The whole pipeline runs synchronously on the blocking pool; query
  embedding blocks in place and is memoized by bbox-embed's process-wide
  query cache (keyed by exact encoder: provider+query_model+dim+dtype+query
  — a repeat query across vector routes embeds once). Don't introduce async stages mid-pipeline, and don't
  re-add per-call embedding dedup maps — the cache is the dedup.
- `discover_seed` reuses `hybrid_search_typed` verbatim and differs only in
  post-processing (notable edges). Ranking changes land in one place and
  affect both; do not fork the ranking for one surface.
- `vector_ranked_lists` searches TWO route families against the same
  `partitions` map from `vectors::try_metrics()`: `Bucket`-keyed text routes
  (`route_buckets`) and chunk-kind-keyed visual routes
  (`EmbeddingRouter::configured_visual_routes()`, `[embed.routes.visual]`).
  A partition matching neither falls through to the pre-existing
  "no configured bucket maps to this partition" skip — that is also the
  entire behavior when no visual route is configured, so the visual lane is
  strictly additive. `configured_visual_routes()` dedupes by partition id
  (image/pdf_figure sharing one multimodal alias search once); the visual
  query embeds through `query_cache::embed_query_cached_visual`, the same
  process-wide cache as the text lanes. doc_type scoping needs no visual
  special case: visual chunks are `project_file` entities (chunk_kind, not
  doc_type, distinguishes them), so `doc_type="project_file"` already
  includes them by prefix match.

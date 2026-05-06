# H1 + critical E3 fixes review

Commits `a0c5362..ae79496` (3 E3 critical fixes + 5 E3 standard fixes + H1).

## Issues (fix-forward)

1. **`features_from_bm25` only extracts features from BM25 hits.**
   Entities found ONLY via vector search (not surfaced by BM25)
   get `RerankFeatures::default()` and end up with rerank
   multiplier 1.0× across the board. So a knowledge entry that's
   semantically relevant but BM25-invisible never gets the
   `UserConfirmed × 1.35` boost. Fix: enrich features for ALL
   fused entity_ids, not just BM25 hits. Walk the fused result
   set, look up each entity_id in tantivy or knowledge store
   regardless of source.

2. **`compact_entity_label(entity_id)` fallback for non-BM25 hits**
   doesn't use `entity_loader::compact_label`. So vector-only
   results get a raw entity-ref-based label instead of the entity's
   title. Same root cause as #1 — non-BM25 entities aren't
   enriched. After fixing #1, the label should derive from loaded
   properties via the existing entity_loader pattern.

3. **Combined rerank multiplier can stack to 1.69×.** type_multiplier
   for knowledge UserConfirmed is 1.35 × temporal_decay for fresh
   knowledge can be up to 1.25 (recall_boost + recency_boost
   stacked), product = 1.69×. The design specified these as
   independent factors but the stack may over-promote young
   confirmed knowledge against everything else. Either:
   - Cap the combined multiplier at 1.50.
   - Use additive composition: `base * (1 + (type_mult - 1) + (decay - 1))`.
   - Accept and tune empirically against the eval suite (H3 lands
     the harness; flag for revisit then).

## Concerns

4. **Embed-on-search adds 100-500ms latency per interactive query.**
   `bbox_hybrid_search` calls `vector_ranked_lists` which embeds
   the query through each active route. Voyage embedding is ~200ms;
   Ollama is faster. For an interactive `bbox_search`-style query
   that wants ms-not-seconds latency, this is a regression. Either:
   - Cache query embeddings (LRU keyed on query text, ~1000 entries)
   - Add `vector_only=false` to skip embedding entirely
   - Make embedding async + background-fill with BM25-first results
   Defer; flag for H3 eval if latency surfaces as a problem.

5. **`include_vectors: Option<bool>` defaults true.** That couples
   every search call to the embedding pipeline. Confirm degradation
   semantics: with `include_vectors=false`, the response should
   omit vector_status fields (or mark them n/a) rather than
   reporting empty queues.

6. **`fetch = DEFAULT_FETCH.max(limit * 4)`** — RRF benefits from
   over-fetching, but `4×` is arbitrary. Daystrom's RRF used a
   fixed 100-item per-source fetch; might want to match. Test:
   if user asks for `limit=10`, do top-K results include items
   that needed deep fetching to rank correctly?

## Critical E3 fix observations (donor parity restored)

7. **E3 critical #1 (incremental insertion)** — `Partition::upsert`
   calls `hnsw.push` instead of `rebuild_hnsw`. Per-write cost is
   O(log n) per insert (graph traversal during insertion) instead
   of O(n log n) full rebuild. Bootstrap of 200 chunks no longer
   recomputes the entire graph 200 times. ✓

8. **E3 critical #2 (graph-first search)** — `HnswIndex::search`
   walks the graph: greedy descent through layers via
   `greedy_closest`, then layer-0 BFS via `search_layer` with
   `ef_search` candidates. Excellent recall via the graph instead
   of brute-force scan. ✓

9. **E3 critical #3 (donor recall parity)** — `donor_recall_parity_1000`
   test builds 1000 clustered vectors, runs 25 queries, asserts
   recall ≥ 0.95 vs brute-force baseline. Test passes. ✓

10. **`HnswIndex::build` was substantially rewritten** to match
    donor pattern: items sorted by deterministic level
    (highest first, so top of graph hierarchy gets built first),
    ramped `ef_construction` from 50 → full ef_construction
    over the first 1000 inserts. This is the "slow to build but
    excellent recall" pattern from the donor. ✓

## E3 standard fix observations

11. **E3 fix #71d8abf (batched derived flush)** — slab.bin /
    ids.bin / graph.bin written on debounce, not per write. ✓

12. **E3 fix #da4b5c8 (batched WAL fsync)** — fsync amortized
    via batched writer. Reduces fsync count proportionally to
    batch size. ✓

13. **E3 fix #2d57c23 (PartitionMetrics state)** — Empty/Active
    discriminator landed. ✓

14. **E3 fix #ebda5a6 (HnswOptions::default once)** — local var
    instead of 4× call. ✓

## Nits

15. **`HybridDegraded::vector_errors` and `skipped_partitions`
    are both BTreeMap<String, String>** — could be one map with
    a discriminator. Subjective.

16. **`RRF_K = 60.0` and `BM25_WEIGHT = 0.4` / `VECTOR_WEIGHT = 0.6`**
    are constants. The design said `vector_weight = 0.6` was the
    default but should be configurable. Add to embed config OR
    expose as a `vector_weight` MCP param. Defer; flag.

17. **`features_from_bm25` returns `HashMap<String, RerankFeatures>`**
    — uses HashMap not BTreeMap; ordering doesn't matter for
    this lookup so HashMap is correct.

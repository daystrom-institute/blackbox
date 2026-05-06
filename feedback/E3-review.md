# E3 + E1 fixes review

Commits `2d76d5c..46821ef` (4 E1 fixes + 4 E3 commits).

## Issues (fix-forward)

1. **HNSW search uses brute-force scan, not graph traversal.** Codex
   flagged this in the done note. `HnswIndex::search` iterates ALL
   active vectors with cosine distance, O(n) per query. The graph
   is built (`insert_internal` runs, neighbor lists populated) but
   ignored by search. At v1 scale (≤10k vectors per partition) this
   is fine. Beyond ~50k vectors per partition, search latency cliffs
   sharply. Two follow-ups:
   - Document the cliff in release notes ("HNSW graph traversal
     deferred; search is brute-force until ~50k vectors per partition").
   - Add a `tracing::warn!` when partition's `active_count` exceeds
     50k so the operator sees the warning before users notice.

2. **`Partition::rebuild_hnsw` rebuilds the ENTIRE graph on every
   upsert.** `Partition::upsert` calls `rebuild_hnsw()` which calls
   `HnswIndex::build(items, ...)` from scratch. That's O(n log n)
   per insert → O(n² log n) for n inserts. Bootstrap of 200 chunks =
   ~305k operations purely for graph rebuilds. The HNSW supports
   incremental insertion (`insert_internal` exists) — wire it.
   Concretely: `Partition::upsert` should call
   `self.hnsw.as_mut().map(|hnsw| hnsw.push(id, vector))` instead of
   rebuilding. Test: insert 100 vectors and assert `metrics().total_nodes
   == 100` without rebuild churn.

3. **`Partition::write_derived_files` writes slab.bin + ids.bin +
   graph.bin on every upsert.** For burst inserts this is N×3 file
   writes. Batch via the queue's debounce — mark partition dirty,
   schedule a flush after quiescence, write once. The WAL is the
   canonical store; derived files just need eventual consistency.

4. **WAL fsync per record.** `wal::append` calls `file.sync_data()`
   per record. 200-chunk bootstrap = 200 fsyncs. Same batching
   argument as #3. Could batch via a `WalWriter` that buffers and
   fsyncs on debounce. Risk of WAL inconsistency on crash window =
   batch size.

## Concerns

5. **`VectorStore` is a process-global singleton via `OnceLock`.**
   `vectors::upsert(route, ...)` etc. all go through `global()`.
   Tests use `VectorStore::open(tempdir)` directly to avoid the
   global, but any code path calling the module-level `upsert`
   touches process-global state. Recommend: remove the
   module-level free functions; pass `&VectorStore` explicitly to
   callers (E2's queue worker takes a handle). Keep the static
   only for daemon main wiring.

6. **`HnswMetrics` doesn't include any health metric** like average
   neighbor degree or layer distribution. For diagnosing search
   quality issues post-H1, these matter. Defer; flag.

7. **`PartitionMetrics::dims`** is the slab's dim. If slab is empty
   (no vectors yet), dim is 0. Caller should handle empty
   partitions gracefully — `bbox_embed_status` should differentiate
   "0 dims because empty" from "0 dims because broken." Add a
   `state: enum Empty | Active(dims)` discriminator.

## E1 fix observations

8. **E1 fix #1 (HTTP timeouts)** — 60s timeout on both providers
   via `Client::builder().timeout()`. Construction returns
   `Result<Self>`; tests + EmbeddingRouter::route_for updated. ✓

9. **E1 fix #2 (queue rate limit doc)** — comment added pointing
   at E2's queue layer. ✓

10. **E1 fix #3 (retry contract doc)** — doc comment on
    `embed_batch`. ✓

11. **E1 fix #4 (Debug redaction)** — explicit `impl Debug` on
    VoyageProvider + OllamaProvider eliding api_key field. Test
    verifies api_key doesn't leak via `format!("{:?}", provider)`.
    ✓

## Nits

12. **`WalRecord::model` and `route` are duplicated** — both store
    the route string. The "model" name is misleading since it's
    actually the route id, not the model name. Rename or
    consolidate.

13. **`write_f32_file` writes f32 LE bytes but `read` for slab.bin
    isn't shown** — the WAL is the canonical source, slab.bin is
    rebuildable. So slab.bin reading might never be used. If so,
    skip writing slab.bin entirely; rebuild from WAL each startup.

14. **`HnswOptions::default()` is called 4 times in `write_derived_files`**
    for the meta.json fields. Compute once into a local. Trivial.

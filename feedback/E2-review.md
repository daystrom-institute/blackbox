# E2 + D3 fixes review

Commits `ae9dddb..ad592fb` (4 D3 fixes + E2 embedding queue).

## Issues (fix-forward)

1. **`seen_hashes: HashSet<String>` grows unbounded.** Every
   `route:chunk_hash` ever seen stays in memory. For a daemon that
   indexes a growing corpus over months, this is a slow memory leak.
   Either:
   - LRU bound (e.g. ~100k entries; evict on overflow).
   - Persist + rebuild from WAL on startup (E3 lands the WAL; the
     dedup source can be derived from existing vector records'
     content_hash field).
   The second is cleaner once E3 ships — defer with a `// TODO(E3+):
   replace in-memory dedup with WAL-derived seen-hashes` comment.

2. **Retry head-of-line block.** When a batch fails and gets parked
   in `retry_batch`, the worker loop checks `retry_batch.is_empty()`
   first; if not empty, it retries the SAME batch and skips
   collecting new requests. So a permanently-failing batch starves
   all subsequent enqueues for that route. Either:
   - Drop the batch after N retries and surface the failure in
     `last_error`; resume normal queue processing.
   - Park the failed batch but ALSO collect new requests; merge them
     when retry eventually succeeds.
   Pick one; current behavior is a pothole.

3. **Rate limiter is per-batch sleep, not token bucket.** `apply_rate_limit`
   sleeps `60_000ms / rate_limit_per_min` BEFORE each batch. So
   `rate_limit=100` → 600ms between batches regardless of batch
   size. Effective throughput depends on batch shape rather than
   request count. For the Voyage 100/min limit you actually want
   100 REQUESTS per minute (or 100 input items per minute,
   depending on Voyage's accounting). A token bucket counting
   actual items would track API quota correctly. Defer; flag.

## Concerns

4. **No graceful shutdown.** Worker's `Shutdown` command exists but
   nothing in main.rs sends it. Daemon termination just drops the
   channel; worker exits via `rx.recv() → None`. Pending batches in
   `pending` deque are lost. For dirty shutdowns this is fine; for
   clean shutdown (SIGTERM with grace), wire a shutdown signal that
   sends `Shutdown` to all workers + waits for them to drain.

5. **`FailingProvider` for unavailable routes** — clean pattern.
   But the failure message comes from CONSTRUCTION-time errors
   (e.g. missing API key), not RUNTIME errors. So `bbox_embed_status`
   for a route with missing VOYAGE_API_KEY shows
   `"VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required..."` —
   useful. ✓

6. **Per-route worker tasks are spawned at startup** via
   `EmbedQueueHandle::start_default`. Routes are derived from
   `Bucket::ALL` — 6 buckets → 6 worker tasks. Per-project route
   overrides aren't pre-spawned (would be combinatorial); they're
   resolved at enqueue time via `resolve_route`. Potential issue:
   per-project routes that route to a different provider than the
   global default never get their own worker — the enqueue
   attempts to send to a non-existent worker. Verify
   `ensure_sender` handles this case (lazy-spawns a worker for the
   per-project route). If not, per-project routes silently fail.

## D3 fix observations

7. **D3 fix #1 (LRU path cache)** — added `accessed_at` counter,
   on `get` updates the path's accessed_at, on overflow sorts by
   accessed_at and evicts oldest 30. Test confirms accessed P1
   survives. ✓

8. **D3 fix #2 (degraded unresolved refs)** — `bundle_evidence`
   now collects unresolved refs into `degraded.unresolved_entity_refs`
   and continues processing the rest. Returns `error.not_found` only
   when ALL refs fail. ✓

9. **D3 fix #3 (clippy 98 baseline)** — `providers/note.rs` and
   `providers/thread.rs` `.into_iter()` issues fixed. Baseline
   restored. ✓

10. **D3 fix #4 (edge_types doc)** — tool description updated. ✓

## Nits

11. **`worker_loop` clones the entire batch into `retry_batch` on
    failure** — N requests cloned per failure. For large batches
    (200+ requests) this is wasteful. Could move the batch back
    into `pending` and re-build it on retry. Subjective.

12. **`sanitize_error` truncates to 200 chars + first line.** Voyage
    error responses can be longer; flag if first line is enough
    context for debugging. Probably fine.

13. **`Bucket::ALL` constant added in this phase** (referenced in
    `start_default` worker spawn). Check that it lists all 6
    buckets in the same order as the enum definition.

14. **`tombstone` is a no-op for now** but K1 + project_files +
    git_history all call it. When E3 lands, those tombstones need to
    actually delete the vector record (mark deleted in WAL). Wire
    in E3.

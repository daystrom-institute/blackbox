# PERFORMANCE-SAGA.md

A walk through the disk-I/O regressions surfaced during the
agentic-corpus arc, what each fix shipped, what was wrong about each
fix, and where the actual bottlenecks turned out to be. Written for
the next person who looks at heavy disk pressure and wonders "is it
the slab? the WAL? tantivy?" — read this before you spend hours like
I did.

## Symptom that started everything

User triggered `bbox_reembed(route="code")` + `bbox_reembed(route="git_message")`
to backfill ~83k Voyage embeddings (~67k code chunks + ~16k commit
messages). Daemon then ran for **5 hours 40 minutes wall clock** with:

- 45% one-core sustained CPU
- **400+ MB/s sustained disk read for the entire window**
- 20 GB peak resident memory
- Nothing else changing on the machine

User reported "my machine was fully pinned the entire time." On NVMe
this is ~10% of raw bandwidth, but 415 MB/s × 5h40m = **8.5 TB total
volume** — enough to evict the page cache repeatedly, drive mmap
re-paging, and queue-depth other I/O behind bbox.

## Round 1: throttle slab.bin flush

**Hypothesis**: `Partition::flush_derived_files` was called from both
`Partition::upsert` AND `Partition::upsert_batch`, and on each call it
rewrote the entire `slab.bin` (~320 MB at 80k vectors × 1024 dims × 4
bytes). 1300 batches × 320 MB = ~414 GB written → page-cache thrash →
sustained reads on mmap'd tantivy pages.

**Fix shipped** (commit `d8cd57d`):
- `last_flushed_wal_records` field on `Partition`; `flush_derived_files_throttled`
  skips when `wal_records - last_flushed_wal_records < FLUSH_MIN_RECORDS = 256`.
- `spawn_periodic_flusher` thread runs every 30s and force-flushes any
  partition with `needs_flush()`.
- `VectorStore::flush_all` for shutdown.
- SIGTERM hook to call `flush_all` before exit.

**Codex review caught**: 256 records was too low — flush every 2
batches → ~325 flushes × 320 MB = 100 GB writes. Plus several
secondary issues:

1. `write_f32_file` made one syscall per f32 (81M syscalls per flush).
2. `VectorSlab::contains_active` and `upsert` did O(n) linear scans —
   83k requests × 80k entries = 6.6B comparisons during a backfill.
3. SIGTERM hook called `flush_all()` synchronously after `axum::serve`
   returned, but `flush_all` blocked behind partition write locks held
   by mid-flight embed workers. systemd waited 90s, SIGKILL'd.
4. Periodic flusher held the partition write lock for the full slab.bin
   write — search blocks behind it. (False positive — search hits HNSW
   in RAM, never reads slab.bin.)

## Round 2: bigger threshold, bulk write, slab dedup index, SIGTERM-safe

**Fix shipped** (commit `546f950` + earlier `d8cd57d`):
- `FLUSH_MIN_RECORDS` 256 → 8192. Periodic timer (30s) drives steady
  state; threshold is just a burst cap.
- `write_f32_file` now bulk-encodes one Vec<u8> + one syscall.
- `VectorSlab` carries `active_index: HashMap<entity_id, idx>`, rebuilt
  lazily after deserialize. `contains_active` / `upsert` / `delete` are
  O(1).
- SIGTERM hook now spawns flush_all on a thread with a 5s deadline.
  Drops on the floor if it doesn't finish; cold start does
  rebuild_from_wal which is correct (slow but bounded).
- Periodic flusher logs at INFO with `slab_bytes` + `elapsed_ms`.

**User retested**: still pinned. 400 MB/s sustained even with these.
Daemon SIGTERM hung 90s and got SIGKILL'd anyway because the flush
thread couldn't finish in 5s on the busy disk.

**Forensic check** revealed the real shape:

```
$ find ~/.local/state/blackbox/vectors -newermt 'window' -printf '%TT %s %p\n'
13:25:40 4448918 ids.bin            (~4 MB)
13:25:41 134     graph.bin          (134 bytes)
13:28:06 675692886 records.wal      (~675 MB JSON-encoded vector WAL)
13:28:17 215203840 slab.bin         (~215 MB)
13:28:17 279     meta.json          (~280 bytes)
```

675 MB WAL. JSON encoding of f32 vectors at ~12 KB per record vs ~4 KB
binary. PER-BATCH `wal::append_many` did 256 syscalls (no buffer) +
`sync_data` (forced disk sync). On NVMe a force-sync is ~100 µs but
still drops cache pressure visibly when running continuously.

## Round 3: WAL stops fsyncing per batch, kill dead derived files

**The clearest finding from Round 2's forensic was structural**:

```
$ grep -r 'slab.bin\|ids.bin\|graph.bin' src/
src/vectors/mod.rs:579-642   (writes)
src/vectors/mod.rs:62-619    (comments)
[no read sites — none]
```

**Cold start always uses `Partition::rebuild_from_wal`.** None of the
derived files (slab.bin, ids.bin, graph.bin) are ever read. We were
writing 215+ MB of files that no consumer ever opened. Dead bytes.

**Fix shipped** (commit `ab89621`):

- `flush_derived_files` now writes only `meta.json` (~1 KB, operator
  visibility) + fsync the WAL. That's it. ~2 KB per partition per
  flush.
- Dropped `slab.bin` / `ids.bin` / `graph.bin` writes entirely. The
  `flush_derived_full` function is now identical to `flush_derived_files`
  (kept as a name for future use if a snapshot file becomes worthwhile).
- `wal::append_many` wrapped in BufWriter (64 KB capacity) — 256
  syscalls per batch → 1.
- WAL no longer fsyncs per batch. Tracks dirty WAL paths in a HashSet.
  Periodic flusher and shutdown call `wal::sync_pending` to checkpoint
  every dirty WAL with one fsync each. Crash window: up to 30s of
  vector writes (rederivable from source).

**User retested** (5311 docs):
- Periodic flush: 7ms (was 1898ms).
- WAL appends throttled, no fsync between checkpoints.
- Drained cleanly, "much better — sporadic bursts but expected."

## Round 4: voyage 400s on dense markdown chunks

**New issue surfaced during Round 3 retest**: `voyage embedding request
failed: HTTP 400 Bad Request batch_size=23 body={"detail":"Request to
model 'voyage-code-3' failed. The max allowed tokens per submitted
batch is 120000. Your batch has 168..."}`

The embed-queue `MAX_BATCH_BYTES = 200 KB` was conservative for typical
text but **markdown chunks dominated by single-char tokens (backticks,
hyphens, code-fence boundaries) tokenize at ~1 char / 1 token in the
worst case**. 200 KB of markdown can be 200k tokens — well above
voyage's 120k cap.

**Fix shipped** (commit pending — see end): `MAX_BATCH_BYTES` 200 KB →
100 KB. Worst-case batch ≈ 100k tokens, comfortably under voyage's
120k.

## Round 5: tantivy segment merges

**Round 3 didn't fully eliminate the disk pressure**. After the
WAL+slab fix the user reported "much better" briefly, then "just
pinned" within a minute. The forensic earlier said 13 tantivy segment
files were touched in the test window. Default `LogMergePolicy` fires
on every commit; the auto-reindex thread commits every 120s and does
post-ingest commits inline. Merging a 1.3 GB index reads + writes
multi-GB in the background.

**Fix shipped** (commit pending): `writer.set_merge_policy(NoMergePolicy)`
on the auto-reindex writer and on the incremental `build_index` path.
Manual `bbox_reindex(full=true)` keeps the default policy so explicit
operator rebuilds also compact segments.

Trade-off: segment count grows over time, search latency degrades
slightly. Reasonable in exchange for predictable ingest I/O.

## Where the bytes actually went, in retrospect

**During the original 5h40m / 8.5 TB run, the contributing sources were
roughly**:

| Source | Estimate per ingest minute | Total over 5h40m |
|---|---|---|
| `slab.bin` rewrite per batch (Round 1's bug) | ~3-5 GB | ~1-2 TB |
| Tantivy segment merges (background) | ~3-5 GB | ~1-2 TB |
| `records.wal` JSON appends + per-batch fsync | ~50 MB | ~20 GB |
| `meta.json`, `ids.bin`, `graph.bin` (small but per-batch) | ~10 MB | ~5 GB |
| Page-cache repaging from mmap'd tantivy | ~5-10 GB (read) | ~2-3 TB |

The last row is the multiplier — every dirty page evicted forces a
re-read on next access. If bbox + tantivy are jointly writing 6-10 GB
per minute and the page cache is ~10 GB, the cache turns over every
1-2 minutes and most of the I/O budget goes to repaging.

## State at end of saga

Committed to main (in order):
- `d8cd57d` — slab.bin throttle + bulk write + slab dedup + SIGTERM
- `546f950` — SIGTERM-safe flush_all + INFO logging
- `ab89621` — drop dead derived files + bufWriter+checkpoint-sync WAL
- pending — MAX_BATCH_BYTES 100 KB + NoMergePolicy on incremental writers

Still open (deferred items in `thread-3e2a0cfa`):

1. **Replace JSON WAL with binary** (~3× size reduction). The current
   WAL is 647 MB at 80k records; bincode/postcard binary would land
   it around ~220 MB. Cleaner cold-start replay too.
2. **Or: replace WAL with rocksdb / sled**. Append-only LSM with
   throttle-able compaction, per-record binary keys, partial reads.
   Bigger refactor but the right long-term answer.
3. **Atomic-rename for derived file writes**. We don't write slab.bin
   anymore but if a future snapshot file lands, write to `tmp` +
   rename to avoid partial-write corruption.
4. **Manual `bbox_index_merge` MCP tool**. Lets operators schedule
   tantivy compaction explicitly during quiet windows rather than
   discovering it the hard way.
5. **systemd `IOWeight=` + `CPUQuota=`** in `deploy/blackbox.service`.
   Belt-and-suspenders so future regressions can't fully pin a
   workstation.

## Lessons

1. **`grep -r 'filename' src/` is the highest-leverage diagnostic** for
   "is this file actually used?" Should have been step 1, not step
   N. Half the saga was throttling writes to files no one read.

2. **mmap + sustained writes = page cache thrash**. NVMe bandwidth
   isn't the bottleneck; cache eviction latency is. A daemon writing
   even 5 GB/min of dirty pages can pin a multi-TB SSD.

3. **The forensic command `find $DATA -newermt 'window' -printf '%TT
   %s %p\n'`** is the single most useful command for this category
   of bug. It shows exactly which files were touched and how big.
   Shipping a logging instrumentation around it (the INFO flush
   logging in commit `546f950`) was the second-most useful move.

4. **Default merge policies in any background-merging system
   (tantivy, lucene, lsm-trees) are tuned for write-heavy
   short-running workloads.** Long-running daemons with intermittent
   ingest want explicit policies. Should be set explicitly on every
   writer creation, not left to defaults.

5. **JSON for hot-path serialization is rarely the right call.** WAL,
   shared queues, large batches — binary or columnar formats scale.
   JSON is fine for low-frequency human-debuggable artifacts (config,
   meta, knowledge entries). Not for per-batch hot loops.

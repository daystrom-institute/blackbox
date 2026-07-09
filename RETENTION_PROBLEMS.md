# Retention & Unbounded-Growth Problem — Design Request

**Status:** request-for-design (not yet a design doc). Hand-off brief for a
follow-up modelling pass. Captures a diagnosed production problem plus the
open design forks; does **not** prescribe an implementation.

**Diagnosed:** 2026-07-09, against the live `blackbox.service` daemon on the
Manjaro host (`~/.local/bin/blackboxd`).

---

## 1. Problem statement

Blackbox has **no retention or windowing policy on its primary indexable
inputs.** Those inputs — transcripts above all, plus registered project source
and git history — are **append-only and unbounded**. Every projection built
off them (the tantivy index, the resident edge graph, the vector store) grows
**monotonically** and nothing ever evicts.

The failure mode this produces is a daemon whose memory footprint **creeps
upward even while it is barely used**, because the growth is driven by *host*
activity, not *daemon* activity: this machine runs 3 Claude accounts across
~17 projects, generating new transcript sessions continuously. The reindex
thread and embedding routes ingest that stream regardless of whether anyone
ever queries this daemon. So "idle" is the wrong mental model — the corpus
never stops arriving.

This is **not a classic leak** (no forgotten `free`, no growing handle set —
fd count was a healthy 50). It is unbounded working-set growth by design, with
a large allocator-fragmentation multiplier layered on top.

---

## 2. Evidence (as measured)

### Process footprint (pre-restart, PID 960, up 8 days, 2h31m CPU total)
- `VmRSS` **19.7 GB**, almost entirely **anonymous** (`Pss_File` only 135 MB —
  so this is heap, *not* the mmap'd tantivy index).
- cgroup `memory.current` **43 GB**, `memory.peak` **56.3 GB** this run.
- `VmHWM` 33.5 GB, `VmSwap` 3.25 GB, `AnonHugePages` 18.5 GB.
- **167 anonymous maps in the 64–140 MB range** — the glibc per-arena
  fragmentation signature (≈40 threads, `MALLOC_ARENA_MAX` unset → up to
  8×ncpu arenas that glibc never returns to the OS).
- The committed-vs-live gap (**56 GB committed vs ~20 GB live**) is the
  fragmentation tax: a ~2–3× multiplier on the real working set.

### Trend (confirms creep, not a one-time spike)
- Prior gap note `gap-bcced6fb` (2026-06-22) recorded a *previous* daemon run
  idle at **11.7 GB RSS**. This run had already reached 19.7 GB live / 56 GB
  committed. Monotonic in-run growth on a near-idle daemon.

### Corpus scale (`bbox_stats`)
- **1,001,869** tantivy index documents (24 segments, 1.1 GB on disk).
- **3,927,933** tool-call edges — transcript-derived, held **fully resident**.
- Source files: `claude` 163, `account2` 1442, `account3` 1, `ds` 61,
  `zai` 61, `codex` 1477, … (thousands of session files, append-only).

### On-disk state (`~/.local/state/blackbox`, 67 GB total)
- `edges/` **57 GB**, `vectors/` 9.7 GB.
- `bbox_storage_health` breakdown of `edges/`:
  - `managed_derived` **21.08 GB** (12 files) — active derived edge layer,
    loaded resident. (Written via `replace_project_edges`, truncate + atomic
    rename, so this is *not* runaway append — it is the genuine current
    derived-edge count for large repos, and it grows with history depth.)
  - `inactive_snapshot` **18.19 GB** (28 files) — per-HEAD-commit
    `materialized/.../snapshots/head-*/project.jsonl`, 1.7–6.5 GB each.
  - `backup` **6.61 GB** (6 `.bak-*` files, timestamps ~mid-May, ~2 months old).
  - `active_legacy` 2.99 GB, `orphan` 56 MB.
  - **`status: "ok"`** — the health check has no leak-vs-steady-state signal.
- Vector store (`vectors/voyage-voyage-code-3-1024-.../`):
  `records.wal` **5.96 GB** is **larger than** `snapshot.bin` 4.19 GB — the WAL
  is not being compacted/truncated after snapshotting. `slab.bin` 215 MB.

### Existing retention (only on *secondary* stores — none on primary inputs)
- `response-dumps` 7 d (`src/server/response.rs` `DUMP_RETENTION`),
  roster 24 h, system-event journal/outbox compaction, `storage_gc`
  snapshots/backups.
- **`src/config.rs` has zero index/transcript retention knobs.** No
  age window, no size cap, no TTL on the transcript index, vector store, or
  resident edge graph. (`grep` for `retention|max_age|ttl|evict|window` over
  `src/` + `crates/` returns only the secondary stores above.)

---

## 3. The resident-growth surfaces (where bounding must go)

The ~20 GB live anonymous heap is dominated by structures that grow 1:1 with
the unbounded inputs and are held entirely in RAM:

1. **Resident edge graph** — `EdgeIndex`
   (`crates/bbox-edge-index/src/edge_index.rs:19`) holds:
   ```
   edges: Vec<Edge>,
   forward: HashMap<EntityRef, Vec<usize>>,
   reverse: HashMap<EntityRef, Vec<usize>>,
   commit_anchor_index: HashMap<String, Vec<usize>>,
   session_tool_calls: HashMap<(String, String), Vec<usize>>,
   ```
   All ~3.9 M+ tool-call edges plus per-project code/provenance/git-history
   edges are loaded at boot (`src/server/open.rs`) and **never shed**. This is
   the single biggest resident consumer and it scales directly with transcript
   count and git history depth. Millions of small `Edge`/`String`/`BTreeMap`
   allocations are also what fragments the glibc arenas (§2).

2. **Vector store** — per-chunk embeddings for every transcript chunk. Two
   problems: (a) no eviction of vectors for out-of-window transcripts; (b) the
   WAL never compacts (`records.wal` > `snapshot.bin`), so even the on-disk
   size is unbounded independent of vector count.

3. **Tantivy transcript index** — 1 M docs and climbing. On disk / mmap'd so it
   is not the primary RSS driver, but it is the same unbounded-input class and
   it grows the boot-time edge/graph projection.

4. **Git-history edges** — `git_history.rs` `commit_edges` emits a parent edge
   plus one edge per changed file, per commit. Monotonic with history depth per
   repo. Smaller lever than transcripts but the same growth class.

---

## 4. Two distinct problems — keep them separate

**Root cause — no input retention.** Even a perfect allocator climbs linearly
forever if the inputs never stop. This is the fix that bounds the *trajectory*.
It is a design decision, not a mechanical one (see §6).

**Multiplier — allocator fragmentation.** glibc with no arena cap and no
`malloc_trim` taxes the real working set 2–3×. Fixing it changes the slope's
constant, not the fact that it is unbounded. Still worth doing.

- **Stopgap already applied (2026-07-09):** systemd drop-in
  `~/.config/systemd/user/blackbox.service.d/memory.conf` sets
  `MALLOC_ARENA_MAX=2` + `MALLOC_TRIM_THRESHOLD_=134217728`. Restarting the
  known-good binary dropped the footprint from ~40 GB to 1.8 GB immediately.
  This is a zero-rebuild band-aid.
- **Durable allocator fix (to design/build):** a `#[global_allocator]` on
  jemalloc (`tikv-jemallocator`) with background decay
  (`background_thread:true`, tuned `dirty_decay_ms`/`muzzy_decay_ms`) returns
  freed pages to the OS aggressively and fragments far less than glibc. It
  supersedes the arena-env stopgap (remove the drop-in once deployed).
  Optionally call `malloc_trim`/purge after each reindex cycle
  (`src/server/open.rs` reindex thread) if staying on glibc.

---

## 5. Adjacent fixes surfaced during diagnosis

- **`storage_gc` reclaims nothing at its current defaults.** A dry-run with
  `prune_inactive_snapshots=true` + `prune_backups=true` reported **0
  deletable bytes**: every candidate is retained by
  `snapshot_retained_recent_repo` / `snapshot_retained_recent_workspace`
  (age 14 d, 3/workspace, 10/repo, 16 GB/workspace budgets) or
  `backup_retained(#1,source=…)`. At multi-GB-per-commit snapshot sizes and
  2-month-old backups, the defaults retain ~25 GB of pure disk waste. The
  budgets need to be size-aware, or default `max_backup_age_days` /
  tighter snapshot budgets. (Code: `src/tools/storage_gc.rs`,
  `crates/bbox-edge-index/src/storage_health.rs`.)
- **Vector WAL compaction** (`crates/bbox-vectors`): truncate/rotate the WAL
  after a snapshot so it does not exceed the snapshot it replays into.
- **Process-memory diagnostics** — open gap `gap-bcced6fb`: there is no
  read-only surface reporting daemon RSS/anon/swap or vector active/deleted
  counts, so operators cannot distinguish a leak from steady state without
  scraping `/proc`. Land this so the next regression is caught in-band.

---

## 6. Design forks to resolve (the actual ask)

1. **Which surfaces get a bound.** Transcripts (index **and** resident edges)
   is the dominant lever. Vectors (+ WAL compaction) is ~10 GB. Git-history
   edges and disk snapshots/backups are smaller, same class.
2. **Eviction model.** Options, in increasing build cost:
   - *Size cap, evict oldest* — target a total budget (docs/bytes/resident
     edges); evict oldest-first. Predictable ceiling regardless of activity
     rate.
   - *Hard-drop past a window* — stop indexing / evict older than N days.
     Simplest; permanently loses old-transcript recall from blackbox.
   - *Hot window + cold archive* — keep recent N days hot; move older
     transcripts to a cold store searchable on demand / rehydratable.
     Preserves recall, bounds hot footprint, most to build.
3. **Resident vs paged edges.** Does `EdgeIndex` stay fully resident (and get
   bounded by the transcript window), or move to a lazy/paged/on-disk-backed
   structure so old edges are not held in RAM at all? The former is a smaller
   change; the latter removes the RSS ceiling problem structurally.
4. **Where the window is enforced.** At ingest (don't index out-of-window
   sessions), at load (don't hydrate out-of-window edges into `EdgeIndex` at
   boot), or as a background evictor tick — likely all three need to agree on
   one window definition in `config.rs`.
5. **Config surface.** New retention knobs in `src/config.rs` + env allowlist
   (window length, size caps, per-surface toggles), following the existing
   config-precedence and env-override conventions.

---

## 7. Pointers

- Process footprint: `/proc/<pid>/status`, `/proc/<pid>/smaps_rollup`,
  cgroup `memory.current`/`memory.peak`.
- Edge graph: `crates/bbox-edge-index/src/edge_index.rs`,
  `crates/bbox-edge-sidecar/src/edge_sidecar.rs`, load at
  `src/server/open.rs`.
- Git history projection: `crates/bbox-corpus-index/src/index/git_history.rs`.
- Vector store: `crates/bbox-vectors/src/lib.rs`, `vectors/…/records.wal`.
- Storage GC / health: `src/tools/storage_gc.rs`,
  `crates/bbox-edge-index/src/storage_health.rs`.
- Reindex loop: `src/server/open.rs` `spawn_reindex_thread`,
  `cfg.index.reindex_interval_secs`.
- Config loader / env allowlist: `src/config.rs`.
- Open diagnostics gap: `.bbox/gaps/gap-bcced6fb.json`.
- Allocator stopgap: `~/.config/systemd/user/blackbox.service.d/memory.conf`.

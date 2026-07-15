---
title: "Concurrency model: planes, invariants, and the path off the bolt-on era"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
tags: [concurrency, tokio, locks, persistence, tantivy, actors, backpressure]
brief: "Holistic concurrency architecture for blackboxd: current as-built map, the recurring defect taxonomy, seven target invariants, plane isolation via owner actors, and a phased migration."
---

# Concurrency model: planes, invariants, and the path off the bolt-on era

> **Status.** Partially implemented (same-day campaign, 2026-06-09, waves 1–6
> on `beta/blackbox-v2`; work thread `thread-935b467d` holds the per-wave
> record). LANDED: Phase 0 (status single-lock + result/report budgets, I7
> JSON-safe transport cap, tokio-metrics sampler + /admin/runtime-metrics with
> tokio_unstable detail); Phase 1 (StorePersister actors for notes, threads,
> pins, roadmap, projects, central kb — incl. the wave-6c reload-wrapper
> purge; ~60 heavy sync handlers moved to run_blocking); Phase 3 dispatch
> plane (bounded EventRing + monotonic counter, tee via per-task writer
> thread, allocator/cooldown off-path, /tail decoration caches, harness
> session persist via spawn_blocking) and the §4.5 RosterView. Wave 7 added
> bro_dashboard on RosterView, the poll-time histogram builder opt-in, and
> atomic harness session writes. Wave 8 (2026-06-10) closed the MCP wire-head
> tax: SurfaceDecisionCache (generation-validated, packet-store scans off the
> request path) + idempotent packet-artifact boot restore + duplicate-packet
> GC. Wave 9 landed **Phase 2**: the §4.3 IndexWriterActor
> (src/index/writer_actor.rs) — all in-process tantivy writes serialize
> through one actor thread; the reindex pass executes inside the actor with
> phase-boundary drains; bbox_reindex unified onto the same pass; the
> LockBusy/silent-skip class is structurally gone. §4.3 delta as built: the
> actor creates its writer per batch/pass with retry-on-busy instead of
> holding it for the daemon lifetime (keeps boot initial build + cross-process
> guard semantics; the queue, not the held lock, provides serialization).
> Measured: idle mean poll 1791µs → ~236µs across the campaign; under-load
> samples confounded by host rustc storms (see thread note 22). Wave 12
> (2026-06-10 evening) closed the edge plane: `EdgeIndex::rebuild` split so
> store read guards cover only the in-memory projections (the multi-GB
> sidecar parse — measured 13–109s in prod — now runs guard-free on the
> watcher thread); `bbox_thread` link / project unregister nudge the watcher
> via a coalescing channel instead of rebuilding inline on tokio workers
> (edges go eventually-consistent, ~seconds); gap-store mutations moved to
> run_blocking; snapshot growth root-caused (one ~0.5GB generation per HEAD
> commit, 14d age floor retained a 23.7GB week — maintenance GC default
> tightened to 2d via `BLACKBOX_STORAGE_GC_SNAPSHOT_MAX_AGE_DAYS`, emptied
> snapshot dirs now pruned). Waves 13–15 (2026-06-10/11) closed the
> dispatch-plane streaming residual — in-process tool bodies to the blocking
> pool, sidecar event-log writes to a writer thread, O(chunk) stream-delta
> ingest (no per-delta journal/ring/clone work) — and the §4.6 runtime-split
> question is RESOLVED: not adopted, fixed by invariants (see §4.6 for the
> measured before/after and the revisit trigger). Status-snapshot publication
> is dropped (roster/status stayed ~1ms under every load test). REMAINING:
> Phase 4 enforcement (lint design delivered, unimplemented) and the audit's
> next-tier I2 instances (apply_patch, enter/exit_worktree, whiteboards,
> bro_mcp, gaps persister).
> File:line citations are point-in-time from the original survey at commit
> 9cb5228; verify against code.

> **2026-07 process-topology revision.** The rejection of a second Tokio runtime
> in this document remains valid as a scheduling decision inside one process. It
> did not decide whether corpus, fleet coordination, and harness sessions should
> share a process, build, or restart domain. The
> [process topology](process-topology.md) now distributes the logical planes
> across blackboxd, blackopsd, fleetd, and per-session bro-harness workers. The lock,
> blocking-I/O, owner-actor, backpressure, and snapshot invariants below remain
> binding inside every resulting service.

## 0. Thesis

blackboxd grew by rapid bolt-on: every new subsystem (stores, indexing,
orchestration, workflows, councils, badgey, slack) adopted whatever concurrency
idiom was nearest at hand. The result is **one flat `SharedState` of ~25
independent locks, one shared tokio runtime carrying every plane's work, and
three generations of persistence idiom coexisting** — with the consequence the
operator observes: a high-activity harness instance degrades indexing, which
degrades note/thread resolution, which degrades the control plane, because
nothing isolates them.

The defects are not 25 separate bugs. They are **a small number of
anti-patterns instantiated many times**. The fix is not 25 patches; it is a
short list of invariants plus an ownership model — *each durable resource gets
exactly one writer (an actor); locks guard memory, never I/O; the control plane
reads snapshots, never contends* — applied incrementally, plane by plane.
`TaskPersister` (src/orchestration/mod.rs:81) already proved the pattern on
tasks.json; this doc generalizes it.

## 1. Current model (as-built)

### 1.1 Runtime topology

- **One multithreaded tokio runtime** (`#[tokio::main]`, src/main.rs) carries:
  the axum HTTP control plane (`/control/*`, `/tail`, `/roster`,
  `/orchestrate/*`, `/webhook/*`), the MCP transport (`StreamableHttpService`,
  src/server/mcp.rs), **all ~154 sync MCP tool handlers run inline on worker
  threads** (`Self::run`, src/server/response.rs:67 — no `spawn_blocking`
  anywhere on the tool path), the in-process harness agent loops
  (`spawn_harness_in_process_task` → `bro_harness::agent_loop`), subprocess
  stdout/stderr readers, workflow arc executors, council drain workers, embed
  queue workers, and webhook/poller/cron ingress.
- **Six ad hoc dedicated OS threads**, each bolted on when a pain point
  surfaced: background reindex (src/index/reindex.rs:452), task-persist actor
  (src/orchestration/mod.rs:92), vector warmup, edge-index rebuild watcher
  (src/server/background.rs:21, 60s tick), storage GC, and the .bbox notify
  debouncer.
- No `block_in_place`/`spawn_blocking` usage on any hot path; no runtime
  metrics; no lock-ordering discipline beyond one heroic comment
  (src/server/routes.rs:1787-1799 documents a real A→D→R→A deadlock cycle
  avoided by hand-scoped guard drops).

### 1.2 Three generations of persistence idiom

| Generation | Stores | Idiom | Pathology |
|---|---|---|---|
| 1: lock-everything | notes, threads, kb (central), pins, roadmap, projects | full-store pretty-print JSON + `sync_all` + rename, executed **under the `SharedState` RwLock write guard AND a blocking flock, on a tokio worker** (e.g. src/tools/notes.rs:19 → src/notes.rs:281 → src/json_store.rs:38) | every reader of that store stalls behind an fsync; worker thread blocked; flock has no timeout |
| 2: per-file | knowledge (repo-owned), gaps, packets, badgey proposals/journal, whiteboards, councils | per-item file + atomic rename, narrower locks | better isolation; still fsync on tokio workers |
| 3: journal/actor | task_store (`TaskPersister`), system_events (append-only JSONL + outbox worker) | in-memory mutate under brief lock; coalesced off-thread persist | **the correct pattern** — the only stores with no read-stall-behind-fsync |

### 1.3 Indexing plane

- The background reindex thread holds **tantivy's single IndexWriter for an
  entire pass** (seconds–minutes; full rebuild every ~720 ticks). Sync-path
  mutations open **fresh writers per call** (src/index/knowledge_docs.rs:116,
  :135; src/index/thread_docs.rs:253, :268) and race it → `LockBusy`; the
  reindexer itself silently *skips a pass* on LockBusy, and sync upserts warn
  "will retry on next reindex cycle" (src/server/open.rs:117).
- `sync_knowledge_entry_to_index` (src/server/store_helpers.rs:7) double-locks:
  `kb.read()` then `idx.write()` + fresh writer + commit — on the tool path,
  per knowledge write.
- Edge-index rebuild takes **six store read guards simultaneously** across a
  multi-GB sidecar scan (src/server/routes.rs:1800-1826).

### 1.4 Dispatch/harness hot path (per stream event)

Per event: `task.inner` Mutex (parse, supervision, sink updates — bounded,
correct) → roster broadcast → tail broadcast → fire-and-forget system event.
But also: **`inner.events.push(evt.clone())` with no in-memory cap** (the
50-event cap is persist-only); synchronous tee-file `writeln!` on the tokio
worker per line (src/orchestration/mod.rs:2644); allocator lease lookup +
disruption-cooldown file writes on the event path (mod.rs:2647-2662); and
`bro_status` serializing the event tail **under the same `inner` Mutex the
ingest path needs** (mod.rs:3367-3413), after `task_result_json` already
locked/unlocked once (inconsistent double-read).

### 1.5 Control plane

- `/control/roster` locks **every task's `inner` Mutex in a loop** per poll.
- `/tail` does **filesystem team-history lookups per streamed event**
  (src/server/tail.rs:151).
- `bbox_inbox` holds **five store read guards in parallel** — any single store
  writer (i.e. any note/thread/kb mutation mid-fsync) stalls the whole
  attention surface.
- The MCP 80KB response cap byte-truncates JSON into invalid JSON
  (src/server/response.rs:45) — the original fleet "poller stall" root cause,
  still latent for any large `ok_json` producer.

## 2. Defect taxonomy

Every observed defect — including A1–A4 from thread-935b467d — is an instance
of one of six anti-patterns:

- **P1. Blocking I/O on async workers.** fsync/flock/tantivy-commit/LSP/search
  executed inline in handlers or event callbacks.
- **P2. Lock held across I/O or serialization.** RwLock guard across fsync
  (gen-1 stores); `inner` Mutex across event-tail serialization (`bro_status`).
- **P3. Competing writers for a single-writer resource.** Fresh tantivy
  writers vs. the reindex pass; LockBusy as a *normal* outcome.
- **P4. Stacked guards on read fan-in.** inbox (5 guards), edge rebuild
  (6 guards), roster (N inner Mutexes per poll).
- **P5. Unbounded growth / clone-heavy hot paths.** `inner.events` Vec;
  full-event clones per ingest.
- **P6. Transport-layer truncation of structured data.** The 80KB cap
  producing invalid JSON instead of a structured error.

## 3. Target invariants (the constitution)

These are the rules new code must satisfy and migration drives old code toward:

- **I1 — Locks guard memory, never I/O.** No fsync, flock, tantivy commit,
  network call, or subprocess wait while holding any `SharedState` lock or any
  `task.inner` Mutex. Corollary: no lock held across `.await`.
- **I2 — No blocking I/O on tokio workers.** Disk and IPC work happens on
  owner actor threads or via `spawn_blocking`. Sync MCP handlers that do heavy
  work (search, refactor planning, reindex) become async wrappers around
  `spawn_blocking`.
- **I3 — One writer per durable resource.** Each JSON store file, the tantivy
  index, and each journal has exactly one writing execution context (an owner
  actor). Enforced by ownership (the actor holds the writer/file handle), not
  by convention.
- **I4 — Mutate fast, persist async, ack by class.** In-memory mutation under
  a brief lock; persistence requested from the owner actor. Two durability
  classes: **telemetry** (tasks, system-event fanout, slack continuity) acks
  immediately, write-behind + coalescing; **operator-durable** (knowledge,
  threads, notes, gaps, roadmap, pins, projects, packets, artifacts) acks only
  after the actor reports durable — callers await off-worker completion, so a
  `bbox_learn` that returned ok survives a crash.
- **I5 — Bounded memory, defined overflow.** Per-task in-memory event ring
  (the transcript file is the source of truth for full history); every channel
  bounded or budgeted with an explicit overflow policy.
- **I6 — The control plane reads snapshots, never contends.** Status, roster,
  tail decoration, and inbox are served from materialized views or
  sequentially-taken short reads — never stacked guards, never the ingest-path
  Mutex, never another plane's writer lock.
- **I7 — Size at the producer; the transport never corrupts.** Responses are
  budgeted where they're built; the transport cap, if ever hit, returns a
  *valid* JSON error envelope.

## 4. Target architecture: four planes, owner actors

The diagram below is the target ownership shape within the original blackboxd
consolidation. Under the process topology, control and dispatch move primarily
to blackopsd, fleetd, and bro-harness workers, while corpus stores and indexes
remain in blackboxd. Plane messages that cross processes use typed RPC; messages
within a process continue using owner actors and bounded channels.

### 4.1 Plane map

```
┌────────────────────────────────────────────────────────────────────┐
│ CONTROL PLANE (axum + MCP, tokio)                                  │
│  reads: materialized views (roster view, status snapshots,        │
│  store snapshots) — wait-free w.r.t. other planes                 │
└──────────────┬─────────────────────────────────────────────────────┘
               │ commands (mpsc)                 ▲ view updates
┌──────────────▼──────────────┐  ┌───────────────┴────────────────────┐
│ DISPATCH PLANE (tokio)      │  │ STORE PLANE                        │
│  harness loops, subprocess  │  │  in-mem stores (brief RwLock) +    │
│  readers, workflow arcs,    │  │  per-store persist via owner       │
│  supervision; per-task      │  │  actor(s); durability-class acks   │
│  ingest → bounded ring +    │  └───────────────┬────────────────────┘
│  status snapshot publish    │                  │ index ops (mpsc)
└─────────────────────────────┘  ┌───────────────▼────────────────────┐
                                 │ INDEX PLANE                        │
                                 │  ONE IndexWriter actor thread:     │
                                 │  owns tantivy writer for life;     │
                                 │  reindex passes are jobs on it;    │
                                 │  small upserts interleave at       │
                                 │  commit boundaries. embed queue    │
                                 │  + vector store unchanged.         │
                                 └────────────────────────────────────┘
```

### 4.2 Store plane: generalize `TaskPersister`

A generic `StorePersister<S>` actor (one thread, or one thread multiplexing all
gen-1 stores — they're small): callers mutate in memory under a brief write
guard, snapshot/serialize *the changed store* off-guard, and hand the actor a
persist request. Coalescing for telemetry-class; ack-channel (the
`flush_blocking` shape that `TaskPersister` already has) awaited via
`spawn_blocking`-friendly async wrapper for operator-durable-class. The flock
stays — but it is taken **only on the actor thread**, so contention can no
longer pin a tokio worker. Batch operations (`bbox_note_resolve` over N notes,
gap-a4e13310) become one mutate + one persist.

### 4.3 Index plane: the writer actor

One `IndexWriterActor` thread owns the tantivy `IndexWriter` for the daemon's
lifetime. Everything that writes tantivy becomes a message:
`UpsertKnowledge(entry)`, `DeleteKnowledge(id)`, `UpsertThread(t)`,
`UpsertThreadsStore`, `RoadmapSync`, `ReindexPass{full}`. The reindex pass runs
*as a job inside the actor* and **drains queued small ops at its existing
commit boundaries** (the 500-file `commit_progress` points), so a knowledge
write enqueued mid-pass lands within seconds instead of failing LockBusy and
waiting for the next cycle. LockBusy ceases to exist as a normal outcome
(remaining only as the cross-*process* guard). `sync_knowledge_entry_to_index`
becomes enqueue-and-return; the embed queue path is already decoupled and
unchanged. Edge-index rebuild stops holding six guards: it clones the
edge-relevant projections per store under sequential short reads, drops all
guards, then scans sidecars.

### 4.4 Dispatch plane: bounded ingest, published snapshots

Per-event ingest keeps the per-task `inner` Mutex (it is brief and correct) but:

- `inner.events` becomes a **bounded ring of compacted events** plus counters;
  the full stream goes to the transcript/tee (the tee write moves to a small
  writer task fed by a bounded channel — P1 off the event path; allocator
  lease/cooldown I/O likewise moves off or gets cached).
- Ingest **publishes a status snapshot** (`arc-swap` or
  `Mutex<Arc<StatusSnapshot>>` swapped at event boundaries): the pre-compacted
  recent-event tail + scalars. `bro_status`/`/control/status` serialize from
  the snapshot **without ever touching the ingest Mutex** (I6). Roster: the
  existing `RosterDelta` emissions maintain a daemon-held `RosterView`;
  `/control/roster` serves it without locking any task's `inner`.

### 4.5 Control plane

`bbox_inbox` takes sequential short reads (an attention surface needs no
cross-store consistency point). `/tail` decoration reads a cached team-ref map
maintained on dispatch events instead of per-event file I/O. Heavy sync tools
(`bbox_search`, `bbox_code_*`, `bbox_refactor_plan`, `bbox_reindex`) become
async handlers wrapping `spawn_blocking`. `cap_response_text` gains the I7
JSON-aware envelope.

### 4.6 Explicitly considered: a second tokio runtime

A dedicated runtime for in-process harness loops (hard isolation of the
dispatch plane) was considered and **deferred**. Rationale: today's starvation
is caused by P1/P2 (blocking the workers, contending on locks), not by CPU
saturation from agent loops; once I1/I2 hold, event ingest is microseconds of
in-memory work and a runtime split adds `Send`/state-plumbing complexity
without removing any remaining contention. Decision trigger to revisit:
sustained worker poll-latency degradation under fleet load **after** Phases
1–3 land, measured via `tokio-metrics` (Phase 0 adds the instrumentation so
this is a data decision, not a vibe decision).

**RESOLVED 2026-06-11 (waves 13–15, thread-935b467d): split NOT adopted —
fixed by invariants.** The trigger DID fire on clean compile-free data:
streaming bros produced ~3 slow polls/sec (means 7–33ms, worst worker 92ms)
after Phases 1–3, falsifying the "ingest is microseconds" assumption at real
event sizes/rates. Three successive fixes attributed and removed the cost:
in-process tool bodies to the blocking pool (wave 13), sidecar event-log
writes to a dedicated writer thread (wave 14), and O(chunk) stream-delta
ingest — take-don't-clone message accumulation, O(tail) snippet, store-by-
move, no delta ring storage, no per-delta `task.progress` journal append,
roster throttle (wave 15). Post-wave-15 under identical 2-bro load: means
1.3–6.9ms heavy-phase, worst worker ≤17ms, ~1.2 slow polls/sec, idle
108–141µs with zero >900µs tail. Residual accepted: SSE bursts still
process many chunks per future poll (tokio coop bounds the batch); at
O(chunk) per-event cost this is ≤~17ms worst-case and does not move
control-plane latency (roster stayed ~1ms throughout). Revisit trigger:
worst-worker mean poll sustained >50ms under compile-free fleet load.

## 5. Migration phases (each independently shippable)

- **Phase 0 — instrument + triage fixes.** Add `tokio-metrics` runtime/worker
  histograms + per-lock-site tracing spans for guard hold times. Land the
  thread-935b467d fixes as invariant instances: A1 (single-lock
  snapshot-then-serialize `task_status_json` — I2/I6), A4 (JSON-aware cap —
  I7). These need no new architecture.
- **Phase 1 — store plane.** `StorePersister` generalization; convert gen-1
  stores (notes, threads, kb-central, pins, roadmap, projects) to
  mutate-fast/persist-async with durability-class acks (A3). Convert heavy
  sync MCP handlers to `spawn_blocking` wrappers. Batch note resolve.
- **Phase 2 — index plane.** `IndexWriterActor`; route sync upserts + reindex
  passes through it; commit-boundary interleaving; retire fresh-writer sites
  (A2). Edge rebuild snapshot inputs.
- **Phase 3 — dispatch plane.** Bounded event ring + status-snapshot publish;
  `RosterView`; tee/allocator I/O off the event path; tail decoration cache.
- **Phase 4 — enforcement.** Lock-discipline lint pass: wrapper guard types
  whose constructors debug-assert "no I/O in scope" markers where feasible;
  CI clippy deny on `std::fs` use inside `src/tools/` handlers except via the
  sanctioned actors; doc the invariants in PROJECT.md validation section.

Ordering rationale: Phase 1 before 2 because store-plane stalls hurt the
operator loop (notes/threads/knowledge) daily, and the persister is the
template the writer actor reuses; Phase 3 last among the big three because the
ingest path is *correct* today, merely wasteful — its pain shows mainly under
fleet load, and Phases 0–2 remove the amplifiers first.

## 6. Open decisions

- **One persister thread multiplexing all stores vs. one per store.** Single
  thread is likely sufficient (writes are small and coalescible); per-store
  threads only if a hot store (notes under fleet load) shows queueing.
- **Status snapshot mechanism:** `arc_swap::ArcSwap<StatusSnapshot>` per task
  vs. swapping an `Arc` under the existing Mutex. ArcSwap is wait-free for
  readers but adds a dependency; the Mutex-swap is probably enough given
  ingest cadence.
- **In-memory event ring size** and whether `recentEvents` semantics change
  visibly for `bro_status tail=N` consumers (fleet client expects the current
  shape; budget already truncates, so a ring ≥ budget is shape-compatible).
- **Whether gen-2 per-file stores (gaps, packets, badgey) also route through
  the persister** or keep direct per-file writes behind `spawn_blocking` —
  their isolation is already decent; I2 compliance may be all they need.

## 7. Relationship to existing work

- `thread-935b467d` A1–A4 are Phase 0/1/2 instances; the thread remains the
  execution tracker.
- `gap-a4e13310` (notes fsync + batch resolve) is subsumed by Phase 1.
- [Process topology](process-topology.md) supersedes the in-process placement but
  not these concurrency invariants. I1 through I7 apply independently inside
  blackboxd, blackopsd, fleetd, and each worker.
- [Harness-daemon boundary](../bro-harness/harness-daemon-boundary.md) defines the
  compile and capability constraints across those processes.
- `TaskPersister` + the system-events journal/outbox are prior art for the
  owner-actor pattern; this doc generalizes rather than invents.

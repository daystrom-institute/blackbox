---
title: "Health-probe starvation during code-source ingest"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
tags: [concurrency, tokio, healthz, ingest, flock, i2, observability, incident]
brief: "Root cause of the 2026-08-13 incident where /healthz stopped answering a 5s probe for over two minutes during a multi-project code-source ingest: four provisional knowledge-source handlers wait on an on-disk lock by spin-sleeping on tokio worker threads, parking the whole runtime."
---

# Health-probe starvation during code-source ingest

Vocabulary is `design/daemon-runtime/concurrency-model.md`: planes (control,
dispatch, store, index), invariants I1 to I7, anti-patterns P1 to P6.

## 1. Incident

2026-08-13, a multi-project code-source ingest batch, largest generation
4.6k files / 1.5GB. `/healthz` stopped answering inside a 5s probe timeout
for over two minutes. Kubernetes killed the process mid-activation and tore a
multi-store update. The torn update is fixed separately by the boot
reconciliation sweep; the starvation is what this note addresses.

## 2. `/healthz` is not the problem, and cannot be made cheaper

`src/server/mcp.rs:11-13` is the entire handler for both probes:

```rust
async fn health_probe() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}
```

No extractor, no `State`, no `Extension`. The future is `Ready` on its first
poll. `build_http_app` (`mcp.rs:15-177`) applies zero top-level `.layer()`
and zero `middleware::from_fn`; every middleware in the process is attached
by `.route_layer(...)` inside one of the three merged producer sub-routers
(`code_source.rs:876`, `git_source.rs:323`, `knowledge_source.rs:873` and
`:928`) and therefore applies only to routes declared in that sub-router.
There is no `ConcurrencyLimitLayer`, `LoadShedLayer`, `BufferLayer` or
`Semaphore` anywhere in `src/`. `.with_state(shared)` supplies state only to
handlers that ask for it, and this one asks for nothing.

So the `/healthz` request path contains no lock, no channel, no queue, and no
blocking-pool dispatch. **Nothing on its path can stall it, and there is no
cheaper implementation to move it to.** When it stops answering, the runtime
has stopped polling ready tasks, and the cause is elsewhere in the process.

## 3. Root cause: an on-disk lock waited on from tokio worker threads

Four `async fn` handlers in the provisional knowledge-source lane call a
synchronous helper directly in their bodies, with no blocking-pool hop:

| Handler | Site | Shape |
|---|---|---|
| `provisional_capture_context` | `knowledge_source.rs:938` | polled GET |
| `begin_provisional_upload` | `knowledge_source.rs:1214` | hot, upload begin |
| `finalize_provisional_upload` | `knowledge_source.rs:1307` | hot, finalize |
| `renew_provisional_generation` | `knowledge_source.rs:1359` | periodic lease renewal |

All four reach `current_provisional_capture_context`
(`knowledge_source.rs:941`), which calls `runtime.load_verified(&project_id)`
(`:960`). On a cache miss that is
`AcceptedPublicationRuntime::load_verified`
(`crates/bbox-indexing/src/accepted_publication_runtime.rs:1070-1076`) into
`refresh` (`:1541`), whose first act is:

```rust
let guard = self.lock()?;                     // accepted_publication_runtime.rs:1548
```

`lock` (`:1527`) is `acquire_accepted_publication_lock`
(`crates/bbox-indexing/src/accepted_publication_store.rs:899-914`), which
calls `acquire_store_lock_nofollow_with_timeout` with
`ACCEPTED_PUBLICATION_LOCK_TIMEOUT`, a 15 second budget
(`accepted_publication_store.rs:43`). That waiter is a spin-sleep loop
(`crates/bbox-corpus-core/src/json_store.rs:496-515`):

```rust
loop {
    match file.try_lock_exclusive() {
        Ok(()) => return Ok(StoreLockGuard { file }),
        Err(error) if matches!(error.kind(), WouldBlock | Interrupted) => {
            if started.elapsed() >= timeout { anyhow::bail!("timed out ..."); }
            let remaining = timeout.saturating_sub(started.elapsed());
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        ...
    }
}
```

`std::thread::sleep` on a tokio worker thread. Not a yield, not an await: the
worker is gone for up to 15 seconds, and a `tokio::time` timeout cannot
interrupt it because the worker that would fire the timer is the worker that
is asleep.

### Why that takes the whole daemon down

`src/main.rs:50-68` builds the runtime with `new_multi_thread()` and the
default worker count, one per available core. In a container that is
typically a small number. Each concurrent provisional request that finds the
lock contended parks one worker for up to 15s, so **a handful of them park
every worker in the runtime simultaneously.** With no worker left to poll
anything, the daemon serves nothing: not the MCP transport, not the axum
accept loop, and not `health_probe`, whose readiness is irrelevant when
nothing polls it. This is P1 (blocking I/O on async workers) and P2 (lock
held across I/O) producing an I6 failure (the control plane must never
contend with another plane).

The holder is the publication side of the same lock, which runs correctly on
the blocking pool (`attempt_publisher_auto_advance` is wrapped in
`blocking(...)` at `knowledge_source.rs:1166-1171`) and holds the lock across
the publication. So an ingest batch produces exactly the pairing required:
long lock holds on one side, spin-sleeping worker-thread waiters on the
other. Two minutes of unavailability is a succession of these 15s parks as
each freed worker is immediately re-parked by the next queued request.

This also explains the second reported symptom, "every MCP caller degrades
during ingest windows", without needing a separate hypothesis: MCP tool
dispatch needs a worker too.

### Candidates from the incident brief, resolved

- **(b) a lock shared by the health path and ingest: correct in spirit,
  wrong in location.** The health handler shares no lock with anything (§2).
  The shared lock is between the ingest write path and the ingest *read*
  path; `/healthz` is collateral damage because it needs a worker thread and
  there are none. Chasing a lock on the health path itself would have found
  nothing, forever.
- **(a) large blocking sections on the serving runtime: yes, but small ones
  in aggregate, not one big one.** Each individual park is a lock wait, not
  a hash or a commit. The sanctioned `#[allow]` sites did not grow; the
  offending code was never covered by the gate at all (§5.1).
- **(c) blocking-pool exhaustion: real but secondary.** One tokio blocking
  pool (default 512, never configured: `main.rs` sets only `thread_name`)
  carries both MCP tool handlers (`response.rs:213`, `run_blocking`) and the
  whole producer ingest surface (`code_source.rs:6754`, `blocking`).
  Activation additionally holds a pool thread for its full duration
  including `std::thread::sleep(retry_delay)` up to a 60s cap
  (`code_source.rs:1343-1344`), a 1s staging retry sleep
  (`code_source.rs:5886`), and a park on `ack_rx.recv()` inside
  `IndexWriterActor::stage_collected_generation`
  (`crates/bbox-indexing/src/index/writer_actor.rs:1276`) while the
  single-threaded actor stages the generation. That degrades MCP callers
  under load. It cannot explain `/healthz`, which never touches the pool.
- **(d) one unyielding multi-GB unit: true, and correctly placed.** The
  1.5GB generation is staged as one unit on the `IndexWriterActor` thread,
  which is where §4.3 of the concurrency model intends it. It does not block
  a worker.

## 4. The fix

Relocate the four waits to the blocking pool, which is what every other
store call in that module already does. `src/server/knowledge_source.rs`
gains `blocking_http`, the `HttpError`-preserving twin of the existing
`blocking` helper (`blocking` maps through `HttpError::from_store`, which
would flatten the 409 `knowledge_source_accepted_generation_stale` status
these checks depend on), and the four handlers wrap their currency check in
it.

This is the minimal change that removes the mechanism. It does not restructure
the ingest pipeline, and it does not touch the serving path.

Deliberately not done here:

- **No change to `/healthz`.** It is already minimal (§2), and a separate
  liveness listener would have hidden this bug rather than fixed it: the
  daemon was genuinely unable to serve, and answering `OK` from an isolated
  runtime while every worker is parked would report health that does not
  exist.
- **No blocking-pool partition.** §3(c) is real and worth its own arc:
  isolating the ingest plane from the MCP plane's pool has backpressure
  semantics to settle, and it is not what caused this incident.
- **The remaining I2 violations on this surface are not fixed here.** A
  sweep found more: `catalog_onboard` (`code_source.rs:1202-1203`) reloads
  producer auth and applies source transitions on the worker, which reads
  every producer token file, does a full `read_dir` of the store root plus
  an fsync (`reap_upload_body_tempfiles`), and reads every activation
  record; `git_source.rs:350` and `:467` do three fs syscalls each on the
  provenance export path; and the `tempfile_in(...)` plus `.reopen()` pair
  in every blob PUT (`code_source.rs:971-976`,
  `knowledge_source.rs:1544-1549`, `git_source.rs:609-614` and `:881-886`)
  is two synchronous opens on a worker. None of these spin-sleep for
  seconds, so none of them is this incident, but `catalog_onboard` is the
  next one to fix on absolute park time.

## 5. Two substrate gaps this exposed

### 5.1 The I2 enforcement gate does not cover the ingest plane

Phase 4 enforcement is narrower than it reads:

- `src/lib.rs:7` and `src/main.rs:7` carry a crate-wide
  `#![allow(clippy::disallowed_methods)]`.
- The only place that re-denies it is `src/tools/mod.rs:5`.
- `scripts/lint-concurrency.sh` globs `src/tools/**/*.rs` and nothing else,
  and its two rules are about `#[tool]` handler shape and thread spawns.

The entire `src/server/` HTTP ingest plane, `code_source.rs`,
`knowledge_source.rs` and `git_source.rs`, is outside **both** gates. The
gate was scoped to MCP handlers when the producer transport did not exist in
this shape; the transport has since grown into a second, equally hot request
plane, and enforcement did not follow it. Note also that clippy would not
have caught this one even if scoped there: the disallowed list names
`std::fs::*` and `std::process::Command`, and the offending call is a
`std::thread::sleep` several crates deep behind `load_verified`. The real
gap is that no gate expresses "this async fn body can park".

### 5.2 The runtime telemetry is blind in precisely this window

Three compounding reasons the incident left no usable runtime evidence:

1. **The discriminating metrics are compiled out.** Worker poll durations,
   `blocking_queue_depth`, `blocking_threads_count` and
   `idle_blocking_threads_count` all sit behind `#[cfg(tokio_unstable)]`
   (`runtime_metrics.rs:116-177`), and nothing in the build sets
   `--cfg tokio_unstable`: `.cargo/config.toml` sets only the cold guard and
   a linker flag, `build.rs` emits only the build id, and `Cargo.toml:76`
   merely registers the cfg name for `check-cfg`. In the shipped binary
   `add_unstable_snapshot_fields` is the no-op stub at `:180` and
   `builder.enable_metrics_poll_time_histogram()` (`main.rs:63`) never
   compiles in.
2. **The sampler is starved by the starvation it measures.**
   `spawn_runtime_metrics_sampler` is a `tokio::spawn` on the serving runtime
   (`runtime_metrics.rs:30-40`). When every worker is parked the sampler is
   parked too, so it stops sampling exactly when the numbers matter.
3. **The interval is coarser than the incident.** `DEFAULT_INTERVAL_SECS` is
   60 (`runtime_metrics.rs:8`), so a two-minute event yields about two
   samples, both averaged across the event boundary.

A monitor that shares the fate of the thing it monitors is not a monitor.

## 6. The detector

`spawn_scheduler_latency_probe` (`src/server/runtime_metrics.rs`) measures the
one quantity that names this failure class directly, from outside the runtime:

- It runs on a dedicated OS thread (`blackbox-sched-probe`), the same idiom
  as the other sanctioned daemon-lifetime threads in
  `src/server/background.rs`, using `std::thread::sleep` between probes and a
  `std::sync::mpsc` round trip. No part of its timekeeping depends on the
  runtime being healthy.
- Each probe stamps `Instant::now()`, `Handle::spawn`s an always-ready task
  that sends on a fresh one-shot channel, and measures the round trip. That
  interval **is** the `/healthz` question minus the socket: how long does a
  ready task wait to be polled?
- It records last / max / sample / over-budget counters in atomics, exposed
  at `/admin/runtime-metrics` as a top-level `scheduler_latency` key
  (deliberately not folded into `snapshot`, which is republished by the
  starvable sampler and goes stale in exactly this window). Stable tokio
  only, no `tokio_unstable` requirement.
- It emits a `WARN` per over-budget sample, so a spike lands in the log at
  incident time rather than inside an averaged 60s bucket afterwards.

It shares no lock and no queue with ingest, and adds one trivial task per
interval. Defaults: 1s interval, 250ms budget, tunable via
`BLACKBOX_SCHED_PROBE_INTERVAL_MILLIS` (0 disables) and
`BLACKBOX_SCHED_PROBE_BUDGET_MILLIS`.

Had it been running on 2026-08-13 it would have pinned the diagnosis in one
line: probe latency at multiple seconds means the runtime is not polling
ready tasks, which is §3 and nothing else. If a future window shows
`/healthz` failing while probe latency stays sub-millisecond, the stall is
outside the runtime entirely, and the next places to look are the accept
backlog, cgroup throttle counters (`nr_throttled`, `throttled_time`), and
volume fsync latency.

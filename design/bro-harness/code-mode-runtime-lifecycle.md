---
title: "Code-mode runtime lifecycle and V8 failure containment"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - code-mode
  - daemon-runtime
brief: "Hardens bro-code-mode beneath its existing exec/wait surface with one serialized actor per cell, linearized completion and termination, atomic cross-cell store commits, hierarchical cancellation, preserved yield and dropped-observer output, and explicit session shutdown. V8 remains in-process inside the per-session bro-harness worker, which is the process failure boundary."
---

# Code-mode runtime lifecycle and V8 failure containment

## 0. Decision

Keep the shipped `exec`/`wait` model surface and bro-code-mode's local function
store and namespace work. Replace the current cell lifecycle with a single-owner
actor and explicit terminal-state machine.

Run V8 in-process inside each per-session `bro-harness` worker. Do not build a
separate `bro-code-mode-host` companion in the initial service topology. The
harness worker already isolates V8 from blackboxd, fleetd, and sibling sessions.

The source idiom is Codex main at the
[2026-07-14 snapshot](../../research/harness/codex/codex-main-8aae858958.md),
especially the [metatools finding](../../research/harness/codex/codex-metatools.md).
This design adopts lifecycle invariants while fitting Blackbox's worker
topology.

## 1. Why process isolation is not enough

Moving V8 from blackboxd into a worker contains process crashes, OOM aborts,
embedder faults, and callback UB to one session. It does not fix races among:

- normal completion;
- explicit terminate;
- `wait` observation;
- session shutdown;
- cross-cell state publication;
- a dropped request future;
- an outstanding delegated tool call.

Those semantics belong to a transport-neutral cell actor. They must be correct
before worker restart or replay can make reliable claims about the state that
was lost.

## 2. Cell state machine

Each cell has one actor task that serializes every lifecycle command. Other
tasks send commands and never mutate cell state directly.

```text
Running
  | normal completion
  v
Completed -------> CompletionClaimed -------> Tombstone
  ^                        |
  | terminate loses       | observer consumes terminal output
  |
Terminating --------------------------------> Tombstone
```

The concrete representation may differ, but these semantic states are
load-bearing:

- **Running:** execution may issue nested tool calls and yields.
- **Terminating:** cancellation won; no new session-store commit is allowed.
- **Completed:** immutable terminal output and a pending shared-state commit
  exist.
- **CompletionClaimed:** one observer owns terminal response delivery.
- **Tombstone:** terminal identity and cause remain long enough to answer late
  duplicate calls deterministically without retaining the full runtime.

### Invariants

1. Completion and termination have exactly one linearized winner.
2. A terminated cell cannot publish values to the session store.
3. Successful `store()` updates become visible atomically with successful
   terminal completion, not incrementally during execution.
4. Nested external tool effects are not rolled back by cell termination.
5. `wait` never creates a second completion claimant.
6. Late terminate and wait requests receive stable terminal answers.
7. A worker disconnect from fleetd is not itself cell termination.
8. Worker shutdown closes admission before enumerating and cancelling children.

## 3. Cancellation tree

Use hierarchical cancellation tokens:

```text
worker session token
  +-- provider-turn token
  |     +-- tool-call token
  +-- cell token A
  |     +-- delegated-call token
  +-- cell token B
  `-- fleet connection token
```

The fleet connection token is deliberately not the parent of the session token.
A fleetd restart must not cancel a healthy local provider turn or V8 cell. It
only cancels outstanding RPC calls and starts reconnect behavior.

Session shutdown first closes admission, then cancels accepted children, then
waits for actor termination under a bounded deadline. Admission and shutdown
share one serialized decision point so a cell cannot be accepted after shutdown
enumerates children.

Cell termination cancels V8 execution and pending delegated-call waits. It does
not claim that an already-started shell, network, or other external effect never
happened.

## 4. Observation contract

Output correctness is part of the model-facing API even though the schema stays
unchanged.

- The first `yield_control()` boundary is retained even if execution completes
  before an observer calls `wait`.
- Yielded and terminal output survive a dropped observer.
- Cancelling a request future does not cancel the cell.
- `terminate` is explicit.
- A terminal response includes a stable cause: completed, terminated, worker
  shutdown, runtime failure, worker process loss, or internal protocol failure.
- A fleet reconnect cannot redeliver terminal cell output as a second model tool
  result.

These rules apply to direct `exec` completion and subsequent `wait`.

## 5. Runtime ownership

```text
bro-harness worker process
  |- provider transport and agent loop
  |- filtered ToolCapability registry
  `- bro-code-mode session
       |- cell actors
       |- session store
       `- in-process V8 isolates
```

Cell/session semantics remain a library boundary under the harness loop. Tests
may instantiate the runtime without a live fleet connection, but the production
runtime is owned and shut down by one worker session.

Nested calls resolve through the worker's admitted `ToolCapability`. Local
tools execute in the worker. Fleet or corpus capabilities travel through the
worker's session-scoped RPC client and remain subject to the same tool filter
and policy checks.

V8 never receives an ambient fleet socket, blackboxd address, raw credential, or
unfiltered tool registry.

## 6. V8 supervision

The worker boundary reduces blast radius but does not excuse unsafe embedding.
Each runtime still requires:

- a default-on heap limit and near-heap-limit termination path;
- a cross-thread isolate handle for timeout and explicit cancellation;
- `catch_unwind` or equivalent containment around every host callback before
  control can cross V8 C++ frames;
- bounded cell output and cumulative egress;
- denied ambient globals not deliberately supplied by the host;
- bounded active cells and delegated calls;
- stale-command and stale-resolver cleanup after termination;
- deterministic invalidation of the cell generation after runtime failure.

A fatal V8 failure may terminate the worker. fleetd then applies ordinary
single-session loss and resume policy. It must not attempt to reuse that worker
or trust partially published cell store updates.

## 7. Disconnect and shutdown behavior

| Condition | Required behavior |
|---|---|
| fleetd connection lost | Continue purely local accepted work; remote calls fail retryably or wait by policy |
| fleetd reconnects | Reconcile session commands/events without recreating live cells |
| explicit fleet drain | Stop admitting new turns and cells at the configured safe boundary |
| explicit cell terminate | Linearize against completion and publish one terminal cause |
| worker shutdown deadline expires | Terminate remaining isolates and exit nonzero |
| V8 fatal failure | Worker exits; fleetd marks one session interrupted/resumable |
| blackopsd unavailable | Only routed operational and collaboration capabilities are unavailable |
| blackboxd unavailable | Only routed corpus capabilities are unavailable |

The worker event log records cell lifecycle outcomes needed for diagnosis, but
it does not serialize arbitrary V8 heap state. Resume starts from balanced model
history and durable session state, not a resurrected isolate.

## 8. Optional JIT-less mode

JIT policy is worker-local and session-intrinsic. It may be added after lifecycle
parity:

- persist the selected mode with session configuration;
- validate representative code-mode scripts in both modes;
- report mode in worker handshake and diagnostics;
- never switch mode beneath active cells;
- do not make JIT-less the default without compatibility and performance data.

This is a hardening and compatibility option, not another process boundary.

## 9. Phases

### Phase 1: actor parity

- Extract the transport-neutral cell/session runtime.
- Add actor states and cancellation tree.
- Make session-store commits atomic at successful completion.
- Port race, dropped-observer, initial-yield, and shutdown tests.
- Preserve current in-process V8 ownership while blackboxd still hosts sessions.

### Phase 2: explicit worker ownership

- Bind the runtime to explicit worker session services.
- Separate fleet connection cancellation from session cancellation.
- Add drain and worker-shutdown integration.
- Launch the same runtime inside the bro-harness worker process.

### Phase 3: recovery integration

- Record stable cell terminal causes in worker events.
- Prove fleet reconnect does not duplicate cell results.
- Prove worker loss cannot publish partial session-store commits.
- Integrate single-session resume behavior.

### Phase 4: optional JIT-less mode

- Add configuration, reporting, and compatibility tests.

## 10. Verification contract

At minimum, tests must prove:

- terminate versus completion has one winner under randomized scheduling;
- terminated cells cannot commit `store()` values;
- successful multi-key commits are atomic;
- first yield survives immediate completion;
- a dropped wait caller does not lose output or kill the cell;
- shutdown cannot miss a concurrently admitted cell;
- fleet disconnect does not cancel a local cell;
- blackopsd or blackboxd outage does not cancel a local cell;
- fleet reconnect cannot duplicate a terminal tool result;
- V8 fatal loss affects one worker and cannot publish partial state;
- nested calls still honor explicit tool denies;
- the existing local function-store and namespace behavior is unchanged.

Use `cargo nextest run --workspace` for the mid-cycle gate and the full profile
at closeout, per repository convention.

## 11. Relationship

- [The cell DSL](code-mode-cell-dsl.md) owns value and namespace semantics. This
  document owns execution, observation, cancellation, and V8 lifecycle.
- [Harness-daemon boundary](harness-daemon-boundary.md) owns process and
  capability placement.
- [Worker protocol](worker-protocol.md) owns fleet reconnect and replay. It does
  not serialize V8 heap or cell actor state.
- [Process topology](../daemon-runtime/process-topology.md) makes the harness
  worker the process failure boundary.
- [Remote-worker boundary](remote-worker-boundary.md) owns later off-host
  execution and private filesystem truth.

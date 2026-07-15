---
title: "Process topology: corpus, operations, fleet, and session workers"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - orchestration
  - fleet-tui
tags: [process-boundary, blackopsd, fleet, workers, rpc, restartability, deployment]
brief: "Splits Blackbox into four authority planes plus a thin view: blackboxd owns durable records and corpus truth, blackopsd owns operational intent, fleetd owns live execution, one bro-harness worker owns each session, and bro remains a replaceable client."
---

# Process topology: corpus, operations, fleet, and session workers

## 0. Decision

Adopt a four-process authority model plus a thin operator client:

```text
bro CLI / Fleet TUI ----------------------------> fleetd
operational clients -----------> blackopsd -----> fleetd
                                   |                 |
                                   |                 | full-duplex worker RPC
                                   v                 v
                               blackboxd       bro-harness x N
                               FDR/corpus       provider, V8, tools, context
```

- `blackboxd` owns durable records, transcripts, indexes, and corpus truth.
- `blackopsd` owns agents, workflows, atoms, schedules, and operational intent.
- `fleetd` owns live execution, workers, worktrees, and control truth.
- one `bro-harness` process owns one agent session.
- `bro` owns view-local state and talks to fleetd for live fleet control.

The process boundary is successful only when the components can be built,
replaced, restarted, and recovered independently. Merely moving today's
monolith into another parent process does not satisfy the design.

This decision supersedes the process-placement conclusion in
[the earlier harness-daemon consolidation](../bro-harness/harness-daemon-boundary.md).
The shared contract bottom, pure protocol types, thin fleet client, capability
inversion, and owner-actor concurrency rules survive.

## 1. Authority in one sentence

```text
blackboxd   answers: what happened and what is known?
blackopsd   answers: what should happen next?
fleetd      answers: what is running now?
bro-harness answers: how does this session execute?
bro         answers: how does the operator see and control it?
```

This formulation keeps blackboxd close to its flight-data-recorder namesake and
prevents fleetd from becoming the next orchestration monolith.

## 2. Why split now

The current deployment gives one process five unrelated reasons to restart:

1. corpus, transcript, index, and embedding changes;
2. agent, workflow, atom, and scheduling changes;
3. fleet worker and control changes;
4. provider, agent-loop, and tool changes;
5. V8 and working-set runtime changes.

That coupling makes a routine rebuild or restart of any one area interrupt all
live sessions. It also places provider SDKs, V8, operational state machines,
corpus stores, indexes, and the control server in one compile and failure domain.

The contract-bottom extraction and thin fleet client have removed the hardest
type-level obstacles. The remaining coupling is runtime ownership:

- blackboxd starts the harness as an in-process task;
- capability implementations are installed into process-global harness slots;
- live control uses in-process senders;
- task, operational, and corpus state share one `SharedState` aggregate.

Those are replaceable implementation seams, not reasons to preserve the
monolith.

## 3. Authority map

| Concern | Authority | Why |
|---|---|---|
| Search, embedding, transcript archive, knowledge, notes, threads, provenance | `blackboxd` | Durable FDR and corpus truth |
| Captured run events, packet/artifact evidence, vector and edge indexes | `blackboxd` | Retained and searchable record |
| Logical agents, teams, mailboxes, workflows, atoms, schedules | `blackopsd` | Durable operational intent |
| Webhooks, pollers, crons, whiteboards, publish/integration policy | `blackopsd` | Automation and coordination decisions |
| Live task/session attempts, roster, worker registry, control | `fleetd` | Shared mutable execution truth |
| Worktrees, leases, concurrency, allocation, worker supervision | `fleetd` | Live lifecycle decisions |
| Provider stream, turn loop, context, compaction | `bro-harness` | Session-local execution state |
| V8 cells, local tools, working-copy LSP | `bro-harness` | Must observe the worker's mutable checkout |
| Live session event log | `bro-harness` | Replayable producer record; blackboxd ingests transcript corpus |
| Selection, scroll, composer draft, recall cursor | `bro` | Ephemeral view state |

Placement follows the truth being read or mutated:

- working-set truth lives with the worker;
- live execution truth lives with fleetd;
- durable operational intent lives with blackopsd;
- durable evidence and corpus truth live with blackboxd;
- presentation truth lives with the client.

## 4. Component contracts

### 4.1 blackboxd: FDR and corpus service

blackboxd keeps the records and services whose meaning survives all live agent
sessions. Its public surface includes transcript and event capture, corpus
search and mutation, indexed semantic hints, provenance, and durable knowledge
operations.

Operational definitions may be indexed here for discovery, but blackopsd owns
their meaning and mutation. The live worker owns its append-only session log;
blackboxd ingests and owns the retained, indexed transcript corpus.

blackboxd does not own provider sessions, worker processes, worktrees, live
control, logical agent mailboxes, workflows, schedules, or the Fleet TUI. A
blackboxd restart may delay corpus calls and ingestion, but it must not terminate
an otherwise healthy worker or erase operational intent.

Working-copy LSP does not belong here. An LSP server that must read a mutable
checkout belongs in the worker holding that checkout. Corpus-wide indexed hints
remain in blackboxd and keep their distinct semantic status.

### 4.2 blackopsd: operational-intent service

blackopsd owns durable statements of what should happen:

- logical agents, teams, roles, parent graphs, and mailboxes;
- atom and workflow definitions, versions, state machines, and invocations;
- waits, schedules, crons, webhooks, pollers, and whiteboards;
- requested actions, approvals, operator-authority inputs, and retries;
- artifact publish and integration intent;
- operational MCP tools and policy.

blackopsd requests concrete execution attempts from fleetd and consumes their
outcomes. It does not supervise worker processes or own live task handles. Its
detailed seam is defined in
[Blackops service boundary](blackops-service-boundary.md).

### 4.3 fleetd: live execution service

fleetd is the singleton authority for current work on one host. It owns:

- dispatch attempts, resume, interrupt, steer, model change, compact, and
  closeout mechanics;
- task and session-attempt persistence, roster projection, and transcript paths;
- worker spawn, authentication, leases, health, drain, and resumption;
- live worktree lifecycle and provider/model/profile allocation;
- execution admission, concurrency, and resource policy;
- capability routing between workers, blackopsd, and blackboxd.

fleetd is not a workflow engine, agent database, atom catalog, or corpus daemon.
It brokers calls but does not copy those authorities into its own state.

### 4.4 bro-harness: per-session worker

`bro-harness` returns to being an executable and remains a library only where
that helps tests and code reuse. The production unit is one supervised worker
process per session.

The worker owns provider transport, the agent loop, model-visible context,
compaction, V8 code mode, admitted local tools, working-copy LSP, session side
state, and its event log. It connects outbound to fleetd using the
[worker protocol](../bro-harness/worker-protocol.md).

V8 stays in-process inside the worker. The worker process is already the V8
fault boundary, so a second code-mode companion is not part of the initial
topology.

### 4.5 bro: replaceable view

The CLI and Fleet TUI remain thin clients over `bro-fleet-client`. They render
roster and control state from fleetd and keep only view-local state.

On a same-host deployment the TUI continues tailing the transcript path
directly. Transcript bytes do not need to traverse the control RPC. blackboxd
ingestion is separate from the TUI's low-latency live tail.

Operational and corpus clients may address blackopsd and blackboxd directly for
their respective MCP or operator surfaces. The Fleet TUI still does not become
an owner.

## 5. Capability routing

The worker has one runtime relationship: fleetd. It does not connect directly
to blackopsd or blackboxd.

```text
worker local tool      -> execute in bro-harness
live fleet operation   -> execute in fleetd
agent/workflow/atom    -> fleetd authorizes and forwards to blackopsd
corpus operation       -> fleetd authorizes and forwards to blackboxd
```

The broker centralizes session identity, policy, quotas, audit, and routing.
Large results continue using handles and previews so capability traffic is not
dominated by payload copying.

`bro-capabilities` remains the contract vocabulary. In the worker, session-
scoped RPC clients implement the traits. In fleetd, the broker dispatches each
typed request to a local execution implementation, a blackopsd operational
client, or a blackboxd corpus client. Process-global installed capability slots
are retired.

Absence and outage remain fail-closed. A worker may continue local work while a
remote capability is unavailable, but it cannot silently substitute a weaker
semantic or policy source.

## 6. Compile graph

The intended dependency shape is:

```text
bro
  -> bro-fleet-client
  -> bro-protocol + bro-core

bro-harness
  -> bro-rpc
  -> bro-protocol + bro-capabilities + bro-core
  -> bro-tools + provider transports + V8 runtime

fleetd
  -> fleet-core + bro-rpc
  -> operational/corpus clients + contract bottom

blackopsd
  -> blackops-core + execution/corpus clients + bro-rpc
  -> contract bottom

blackboxd
  -> corpus/index/storage crates + capability/ingest server
  -> contract bottom
```

Final constraints:

- blackboxd does not depend on blackops-core, fleet-core, `bro-harness`,
  `bro-tools`, provider SDKs, or V8;
- blackopsd does not depend on fleet-core, blackboxd stores, or harness code;
- fleetd does not depend on blackops-core, blackboxd stores, or harness code;
- bro-harness does not depend on any daemon implementation;
- `bro-core`, `bro-protocol`, and `bro-capabilities` remain pure;
- transport I/O lives in `bro-rpc`, above the contract bottom.

This graph is what produces independent builds. Separate binaries that still
share implementation crates with high rebuild fanout do not meet the goal.

## 7. Restart and upgrade contract

| Event | Required outcome |
|---|---|
| `bro` exits or upgrades | No effect on any daemon or worker |
| `bro-harness` binary is replaced | Existing workers continue; new workers use the new build |
| One worker exits | Only that execution attempt becomes interrupted or resumable |
| fleetd restarts | Workers reconnect, replay unacknowledged events, and recover controls |
| blackopsd restarts | Live workers continue; new operational decisions and mailboxes pause |
| blackboxd restarts | Workers and durable operations continue; corpus calls and ingestion pause |
| Protocol versions differ | Handshake negotiates a supported version or rejects precisely |

Workers initiate the fleet connection and retry it. blackopsd reconciles
durable operation IDs against fleetd attempt IDs after either service restarts.
blackboxd ingestion resumes from stable producer cursors or event IDs.

An in-flight provider request cannot always be reconstructed after worker loss.
The recovery guarantee is balanced history and resumable session state, not
transparent continuation of arbitrary network streams.

## 8. Deployment units

Each binary is built and installed independently:

- `blackboxd` for corpus, transcript, search, index, and embedding changes;
- `blackopsd` for agent, workflow, atom, schedule, and policy changes;
- `fleetd` for worker supervision, task attempts, worktrees, and control;
- `bro-harness` for provider, tool, context, and V8 changes;
- `bro` for operator-interface changes.

Service management treats blackboxd, blackopsd, and fleetd as separate
long-lived services. Harness workers are fleetd-supervised session processes,
not launchd services.

Rolling worker replacement requires no fleet drain: replace the binary, leave
old processes running, and record build identity during handshake. Daemon
upgrades use their reconciliation behavior from the restart matrix.

## 9. Non-goals

- No separate workflowd, atomd, mailboxd, LSP daemon, or generic service bus.
- No remote-host protocol in the first extraction; use the remote-worker design
  when filesystem truth and network trust differ.
- No transcript RPC for the same-host TUI.
- No synchronous shared event database across all services.
- No attempt to make every tool available during every service outage.
- No silent fallback from semantic or policy-backed capabilities.

## 10. Acceptance criteria

The topology is complete when all of these hold:

1. blackboxd builds without operational, fleet, harness, tool, provider, or V8
   implementations.
2. blackopsd builds without fleet, harness, or corpus-store implementations.
3. fleetd builds without blackops, corpus-store, or harness implementations.
4. replacing the harness binary affects only newly spawned sessions.
5. fleetd restart preserves live workers through reconnect and event replay.
6. blackopsd restart preserves workers and reconciles logical operations.
7. blackboxd restart does not terminate workers or erase operational intent.
8. one worker crash does not affect siblings or any daemon.
9. capability authorization is identical before and after reconnect.
10. `bro` can restart and reattach without owning system state.
11. tests exercise version skew, duplicate commands, idempotent operation
    requests, stale leases, event replay, and ingestion catch-up.

## 11. Relationship

- [Blackops service boundary](blackops-service-boundary.md) owns the line between
  durable operational intent and live execution attempts.
- [Fleet extraction](fleet-extraction.md) is the staged move from today's
  blackboxd ownership to a narrow fleetd.
- [Worker protocol](../bro-harness/worker-protocol.md) defines the reattachable
  control and capability channel.
- [Agent runtime implementation strategy](agent-runtime-program.md) orders all
  four planes with the Codex-inspired runtime, World State, and agent work.
- [Concurrency model](concurrency-model.md) remains the scheduling and actor
  discipline inside each resulting process.
- [Remote-worker boundary](../bro-harness/remote-worker-boundary.md) extends the
  same truth-domain placement to containers and other machines.

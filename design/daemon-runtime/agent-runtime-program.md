---
title: "Agent runtime program: from Codex findings to independent services"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - orchestration
  - context-management
tags: [implementation-strategy, codex, blackopsd, fleetd, workers, code-mode, world-state, agents]
brief: "The connecting implementation strategy for the July 2026 Codex refresh and the Blackbox process split: harden cell semantics, restore bro-harness as a reconnectable worker, extract narrow fleetd and operational blackopsd, slim blackboxd back to its FDR/corpus role, then land World State and native agent tools."
---

# Agent runtime program: from Codex findings to independent services

## 0. Outcome

Build one coherent program from work that otherwise looks like separate
projects:

- the Codex mainline research refresh;
- V8 cell lifecycle hardening;
- model-visible World State;
- model-facing agent and subagent controls;
- bro-harness workerization;
- narrow fleetd extraction;
- blackopsd extraction for agents, workflows, atoms, and schedules;
- blackboxd slimming and independent service lifecycle;
- smaller context, MCP, and disclosure follow-ons.

The outcome is not "copy Codex" and not merely "split the daemon." It is a
session worker that safely hosts the Codex-inspired model-facing runtime,
supervised by a narrow fleet authority, driven by an independent operational-
intent service, and recorded by a durable FDR/corpus service.

## 1. Source and design chain

```text
Codex source snapshot
  |- cell actors, cancellation, World State, agent lifecycle
  |- context-window and MCP catalog details
  `- disclosure experiments
             |
             v
bro-harness semantic designs
  |- code-mode runtime lifecycle
  |- model-visible World State
  `- model-facing agent capability
             |
             v
process architecture
  |- per-session bro-harness worker
  |- fleetd live-execution authority
  |- blackopsd operational-intent authority
  `- blackboxd FDR/corpus authority
             |
             v
incremental implementation and restart gates
```

Research remains point-in-time evidence. The design corpus decides what
Blackbox adopts, changes, or rejects. This program orders those decisions so
later features do not harden the wrong process boundary.

## 2. Program invariants

1. One authority owns each mutable state cell at every migration stage.
2. The contract bottom contains types and traits, never service I/O.
3. The worker owns working-set truth and local runtime state.
4. fleetd owns live execution truth and attempt policy.
5. blackopsd owns durable operational intent and coordination policy.
6. blackboxd owns durable records and corpus truth.
7. Remote capability absence fails closed.
8. Restart behavior is tested, not inferred from binary separation.
9. Model-facing schema stability is preserved unless a deliberate product
   change requires otherwise.
10. Large intermediate values remain outside model context through code mode,
    handles, previews, and bounded returns.
11. Existing operator-authority and semantic-status invariants survive the
    process move unchanged.

## 3. Dependency graph

```text
P0 contracts and restart matrix
        |
        +-----------------------+
        v                       v
P1 cell actor correctness   P2 session-scoped capability clients
        |                       |
        +-----------+-----------+
                    v
             P3 worker parity
                    |
                    v
             P4 reconnect/replay
                    |
                    v
             P5 fleet-core + fleetd
                    |
                    v
             P6 blackops-core + blackopsd
                    |
                    v
             P7 blackboxd capability split
                    |
          +---------+----------+
          v                    v
 P8 World State        P9 native agent tools
          |                    |
          +---------+----------+
                    v
       P10 context/MCP/disclosure follow-ons
```

Cell correctness and session-scoped capability construction can proceed as
separate slices, but both precede production workerization. fleetd must expose a
stable execution contract before blackopsd can leave blackboxd without reaching
back into task internals.

## 4. Milestone 0: contracts and executable probes

Deliverables:

- process ownership and four-service restart matrix;
- worker protocol types and version negotiation;
- operation-to-attempt execution contract;
- session/build identity in status and diagnostics;
- isolated binary-launch test harness using temporary sockets and state roots;
- dependency checks for the intended final compile graph.

No runtime authority changes here. The purpose is to make every later cut
measurable and reversible.

Exit gate: a fake fleet endpoint and real harness worker complete handshake,
exchange a command/event, and fail a version mismatch. A fake execution service
deduplicates an operational request by idempotency key.

## 5. Milestone 1: code-mode semantic foundation

Implement the transport-neutral cell actor from
[Code-mode runtime lifecycle](../bro-harness/code-mode-runtime-lifecycle.md):

- one serialized owner per cell;
- linearized completion versus termination;
- hierarchical cancellation;
- atomic session-store publication at successful completion;
- preserved yield and terminal output across dropped observers;
- stable terminal causes and bounded shutdown.

Keep V8 in the current process during this slice. Moving a runtime must not hide
or reinterpret lifecycle races.

Exit gate: randomized lifecycle tests pass and the model-facing `exec`/`wait`
behavior remains compatible.

## 6. Milestone 2: session-scoped dependencies

Remove implicit process-global construction before changing process ownership:

- introduce an explicit `HarnessSessionServices` or equivalent;
- place `ToolCapability`, corpus, atom, refactor, operations, and future agent
  clients on that session object;
- make tool registration derive from the session's actual capability set;
- preserve fail-closed absence;
- allow in-memory adapters temporarily so behavior does not change yet.

This lets the same harness use in-memory tests, transitional adapters, or final
fleet RPC without changing the agent loop.

Exit gate: two concurrent sessions carry different policies with no global
mutation or cross-session leakage.

## 7. Milestone 3: bro-harness worker parity

Launch one real `bro-harness` process per selected session while blackboxd still
owns tasks and control:

- pass explicit session configuration and working directory;
- retain the current transcript/event-log format and path;
- route events and controls through the worker adapter;
- keep in-process launch as a temporary per-session rollback mode;
- validate API-native providers, tools, V8, compaction, and resume.

This is the earliest point where V8 and provider faults leave blackboxd. Do not
add a second V8 companion process. The harness process is the containment unit.

Exit gate: behavioral parity under the Fleet TUI and one-worker crash isolation.

## 8. Milestone 4: reconnect, replay, and rolling replacement

Activate the complete [Worker protocol](../bro-harness/worker-protocol.md):

- worker-initiated reconnect;
- sequenced event-log replay and durable acknowledgements;
- idempotent command delivery;
- heartbeats, leases, drain, and shutdown;
- session-scoped capability RPC;
- build/version handshake and a supported skew window.

The temporary RPC server still lives in blackboxd. This isolates protocol risk
from service-extraction risk.

Exit gate: restart the temporary owner while a worker is live, reconnect, replay
without duplication, and continue at the next safe session boundary.

## 9. Milestone 5: narrow fleetd authority

Follow [Fleet extraction](fleet-extraction.md):

- extract dependency-clean `fleet-core`;
- move task attempts, roster, workers, worktrees, leases, and live control;
- launch fleetd first in read-only shadow mode;
- cut worker socket and Fleet TUI authority to fleetd;
- move low-level execution/control tools;
- retain no dual writer during the compatibility window.

Exit gate: fleetd restart recovery, TUI parity, worktree lifecycle parity, no
blackboxd-local session sender authority, and no workflow/agent/atom semantics
inside fleet-core.

## 10. Milestone 6: blackopsd authority

Apply [Blackops service boundary](blackops-service-boundary.md):

- define an idempotent execution-request client to fleetd;
- move logical agents, teams, mailboxes, workflows, atoms, waits, schedules,
  crons, webhooks, pollers, whiteboards, approvals, and retries into
  blackops-core and blackopsd;
- move operational MCP surfaces from blackboxd to blackopsd;
- publish operational definitions and outcomes to blackboxd as indexed records;
- keep fleetd authoritative only for concrete attempts and workers.

Exit gate: blackopsd restart preserves live fleet workers and reconciles
durable operations without duplicating accepted attempts.

## 11. Milestone 7: slim blackboxd

Create the typed corpus capability and record-ingest service, then remove
operational and execution code from blackboxd:

- fleetd and blackopsd use typed corpus clients and reconnect independently;
- working-copy LSP and local semantic tools move to the worker;
- catalog/execution hybrids split by truth domain;
- transcript and operational records ingest idempotently from producer cursors;
- blackboxd drops blackops, fleet, harness, tool, provider, and V8 dependencies;
- packaging and service lifecycle become independent.

Exit gate: blackboxd can be rebuilt and restarted while a worker completes a
local-only turn and blackopsd retains durable operations. Corpus calls and
ingestion recover without session loss or duplicate records.

## 12. Milestone 8: model-visible World State

Land [Model-visible World State](../bro-harness/model-visible-world-state.md) in
the worker after session ownership and resume semantics are stable:

1. section coordinator in shadow comparison mode;
2. environment migration;
3. dispatch scope and pins;
4. project instructions;
5. tool-manifest, collaboration, and service-availability sections;
6. retained-fragment reconciliation after compaction and resume.

The worker owns the persisted model-knowledge baseline. fleetd supplies live
attempt policy and availability; blackopsd supplies collaboration and
operational policy. Neither owns rendered model context.

Exit gate: service or worker restarts cannot leave the model believing a stale
tool, agent, permission, or instruction state.

## 13. Milestone 9: native model-facing agents

Implement [Model-facing agent capability](../bro-harness/model-facing-agent-capability.md)
on the new ownership model:

- blackopsd implements canonical identity, teams, mailboxes, logical scheduling,
  and lifecycle intent;
- fleetd implements concrete attempts, workers, worktrees, and live control;
- the worker exposes provider-neutral spawn, message, followup, interrupt, list,
  and wait schemas;
- a session-scoped RPC client implements `AgentCapability` through fleetd;
- World State reports collaboration availability and policy changes;
- user steering interrupts waits at the normal input boundary.

Suggested delivery order:

1. spawn, list, and status;
2. send versus followup typed semantics;
3. non-destructive interrupt;
4. mailbox wait with user-steer wakeup;
5. durable graph/mailbox cursors and cold resume;
6. partial history forks and shared root prompt-cache identity.

Exit gate: blackopsd restart preserves identity and mailbox sequence; fleetd
restart preserves or reconciles attempts; worker restart restores collaboration
state before the next model turn.

## 14. Milestone 10: smaller Codex-derived follow-ons

Only after topology and primary state models stabilize:

- add context remaining as a small read-only tool;
- add explicit context-window lineage after compaction reconstruction has a
  stable window identity;
- reuse sanitized stdio MCP tool catalogs if startup measurements justify it;
- run deterministic skill/lens selection in shadow mode and measure false
  negatives before changing disclosure;
- consider JIT-less V8 as a worker-local policy option;
- defer remote execution environments, clock/sleep tools, generated-image
  helpers, and a generic extension framework until Blackbox has a concrete need.

These features must not reopen process ownership or ambient authority.

## 15. Release increments

| Release | New capability | Authority change | Rollback |
|---|---|---|---|
| R0 | Protocol probes and cell actor | None | Revert additive internals |
| R1 | Harness worker under blackboxd | Session process only | Select in-process mode for new sessions |
| R2 | Reconnect and capability RPC | Control transport | Drain protocol workers |
| R3 | fleetd shadow view | None | Stop shadow service |
| R4 | fleetd authority | Live execution attempts | Drain fleetd-owned sessions before rollback |
| R5 | blackopsd authority | Durable operational intent | Host blackops-core behind the same execution contract |
| R6 | blackboxd capability/ingest service | Corpus calls and records | Compatibility adapter within supported release |
| R7 | Slim blackboxd | Compile and deployment ownership | Forward migration after compatibility removal |
| R8 | World State and native agents | Model-facing behavior | Feature flags per surface, not authority rollback |

Each release has one dominant risk. Avoid combining fleet authority cutover,
blackops authority cutover, protocol redesign, and model-facing schema changes
in one release.

## 16. Verification strategy

### Static gates

- contract crates remain pure;
- forbidden crate edges fail CI;
- protocol schemas have compatibility fixtures;
- corpus frontmatter and links validate;
- stable rustfmt and workspace nextest gates pass for implementation changes.

### Runtime gates

- cell lifecycle race tests;
- per-session capability isolation;
- event replay and command deduplication;
- independent component restart matrix;
- rolling old/new worker coexistence;
- same-host transcript tail continuity;
- blackboxd outage during a local-only turn;
- blackopsd outage while fleet workers remain live;
- fleetd outage during provider streaming;
- one-worker crash under multi-worker load;
- World State restore after compaction plus service restart;
- agent graph and mailbox restore after blackopsd restart;
- operation-to-attempt reconciliation after blackopsd or fleetd restart.

### Live operator probes

Use isolated blackboxd, blackopsd, and fleetd state roots plus a real Fleet TUI
session. Capture exact binaries, protocol versions, provider path, service
restarts, and recovery. Shared production services are not restarted without
the required scope check and operator approval.

## 17. Parallelism and sequencing

Safe parallel workstreams after Milestone 0:

- cell actor semantics;
- explicit session-service construction;
- `fleet-core` and `blackops-core` dependency inventories;
- World State shadow coordinator design and fixtures.

Do not run these authority-changing cuts in parallel:

- worker protocol cutover and fleetd authority cutover;
- fleet attempt ownership and blackops operation ownership cutover;
- blackboxd capability cutover and compatibility-adapter removal.

Parallelize isolated semantic work, but serialize ownership changes.

## 18. Decision ledger

Settled by this program:

- four authority planes plus a thin view;
- one process per harness session initially;
- V8 in-process inside the harness worker;
- worker talks only to fleetd;
- fleetd owns live attempts and brokers blackopsd/blackboxd capabilities;
- blackopsd owns agents, workflows, atoms, schedules, and operational intent;
- blackboxd owns records, transcripts, indexing, search, embedding, and corpus;
- UDS, versioned framed JSON, reconnect, and replay for same-host workers;
- transcript file remains the same-host live-history plane and blackboxd ingests
  the durable searchable corpus;
- no workflowd, atomd, mailboxd, or generic service bus initially.

Still evidence-gated:

- when remote workers become necessary;
- whether a non-JSON wire encoding is worth negotiating;
- how long the worker protocol skew window should be;
- whether later scale justifies worker pooling instead of one process per
  session;
- which smaller Codex features earn implementation after measurement.

## 19. Relationship

- [Codex mainline adoption](../bro-harness/codex-mainline-adoption.md) is the
  feature-level map; this document is the program-level execution map.
- [Process topology](process-topology.md) owns final process authority.
- [Fleet extraction](fleet-extraction.md) owns the live-execution strangler.
- [Blackops service boundary](blackops-service-boundary.md) owns operational
  intent and its extraction.
- [Worker protocol](../bro-harness/worker-protocol.md) owns reconnectable session
  transport.
- [Harness-daemon boundary](../bro-harness/harness-daemon-boundary.md) owns the
  compile and capability constitution.
- [Concurrency model](concurrency-model.md) remains binding inside each service.

---
title: "Fleet extraction: strangling live execution out of blackboxd"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - orchestration
  - fleet-tui
  - bro-harness
tags: [fleetd, extraction, migration, rpc, worktrees, orchestration]
brief: "A reversible extraction plan that restores bro-harness as a supervised process, introduces reconnectable worker RPC, extracts a deliberately narrow fleet-core and fleetd for live attempts, then hands durable agents, workflows, atoms, and schedules to blackopsd before blackboxd sheds execution dependencies."
---

# Fleet extraction: strangling live execution out of blackboxd

## 0. Objective

Move live execution ownership from blackboxd into a standalone, deliberately
narrow `fleetd` without a flag day and without creating two simultaneous
authorities for a session attempt. Move durable operational intent to
`blackopsd`, not fleetd.

The target topology is defined in [Process topology](process-topology.md). This
document owns the migration path, compatibility windows, rollback points, and
deletion ledger.

## 1. Current coupling inventory

The current system already has several extraction-ready seams:

- `bro-core`, `bro-protocol`, and `bro-capabilities` form a pure contract bottom;
- `bro-fleet-client` is a daemon-only thin client;
- roster snapshots and deltas are typed;
- the Fleet TUI receives a transcript path and tails the event file directly;
- `bro-harness` still has an executable target;
- task and event persistence already use owner-style paths rather than relying
  exclusively on terminal memory.

The remaining live coupling is concentrated in four places:

1. blackboxd calls the harness agent loop in-process;
2. live control channels are process-local senders;
3. daemon capability objects are installed into global harness slots;
4. task, orchestration, and corpus state coexist in blackboxd `SharedState`.

The extraction should cut those seams in that order.

## 2. Ownership cut

### Moves to fleetd

- task store, task persister, live task handles, roster view, and tail metadata;
- control routes for exec, resume, cancel, steer, interrupt, model, and compact;
- worker registry, handshake state, leases, acknowledgements, and reconnect;
- resolved execution specifications, not brofile interpretation;
- provider/model/account allocation and concurrency limits;
- worktree creation, seeding, closeout, and cleanup;
- execution attempt and worker resume leases;
- fleet-facing low-level control tools such as `bro_exec`, `bro_resume`, steer,
  interrupt, compact, and cancel;
- capability routing to blackopsd and blackboxd.

### Moves to blackopsd

- canonical agent identities, graph, teams, roles, and mailboxes;
- brofiles and execution-composition policy;
- workflows, state machines, waits, schedules, crons, webhooks, and pollers;
- atom definitions, composition policy, and durable invocation intent;
- whiteboards and shared operational coordination state;
- approvals, retries, integration intent, and operational policy;
- model-facing agent, workflow, atom, and scheduling tools.

### Remains in blackboxd

- corpus and search indexes;
- knowledge, gaps, notes, threads, roadmap, pins, and projects;
- transcript archive, captured run events, and evidence artifacts;
- vector and edge indexes;
- corpus-side semantic services and indexed-hint generation;
- corpus MCP surfaces and durable evidence lookup.

### Splits by operation

Some current subsystems mix catalog and execution concerns:

| Subsystem | blackboxd record/index | Operational and execution owners |
|---|---|---|
| Atoms | Searchable projection and run evidence | blackopsd definition/intent; fleetd attempt; worker step |
| Reusable cells | Indexed artifact evidence | blackopsd operational definition; worker execution |
| Agents | Transcript and searchable history | blackopsd identity/mailbox; fleetd attempt; worker turn |
| Refactor | Corpus/index-backed hints where needed | blackopsd intent; worker LSP, validation, and apply |
| LSP | No mutable-checkout authority | Worker-local session against its checkout |
| Events | Durable searchable record | blackopsd transitions; fleetd live projection; worker log |

Do not preserve an accidental subsystem boundary when its two halves consume
different truths.

## 3. New crate and binary shape

### `fleet-core`

Extract a library containing live execution-domain logic but no HTTP server,
blackops semantics, or blackbox store types. It owns:

- task/session state machines;
- worker supervision and leases;
- roster projection;
- worktree and closeout coordination;
- attempt admission, leases, and capability routing policy.

Its persistence ports are traits or narrow repositories defined at the bottom
of the fleet layer. The initial implementation may reuse existing file formats,
but it must not depend on `SharedState` or blackbox server modules.

### `fleetd`

The binary adds:

- the local worker socket;
- `/control/*`, roster snapshot, and roster stream endpoints;
- low-level fleet control MCP surfaces;
- configuration, service lifecycle, metrics, and health;
- blackopsd and blackboxd capability clients and reconnect policy.

### `bro-rpc`

Transport code shared by fleetd, blackopsd, bro-harness, and blackboxd clients
belongs in an I/O crate above the pure contracts. It owns framing, connection
supervision, multiplexing, and typed client/server adapters.

## 4. Extraction stages

### Stage 0: freeze contracts and ownership

- Land the process topology, restart matrix, and worker protocol.
- Add protocol-version and build-identity types without changing ownership.
- Define one owner for each state cell during every migration stage.
- Add integration tests that can launch isolated binaries on temporary sockets.

Rollback: documentation and unused additive types only.

### Stage 1: process parity under blackboxd

- Replace the common in-process harness launch with `bro-harness` as a child
  process behind the existing task interface.
- Preserve the current event envelope and transcript location.
- Keep blackboxd as the sole task/control authority.
- Run old and new execution modes as a per-session feature choice for parity
  testing, never as joint owners of one session.

This stage immediately moves provider and V8 crashes out of blackboxd while
leaving product routing unchanged.

Rollback: select the existing in-process session mode for new sessions.

### Stage 2: worker RPC and session-scoped capabilities

- Add the outbound worker connection, handshake, commands, event sequence,
  acknowledgements, heartbeat, and reconnect.
- Make the event log the worker's replayable outbox.
- Implement `bro-capabilities` through session-scoped RPC clients.
- Remove process-global installed capability slots from harness construction.
- Keep the RPC server temporarily inside blackboxd.

Rollback: stop admitting reconnectable workers and use the Stage 1 compatibility
transport. Existing protocol sessions drain normally.

### Stage 3: extract `fleet-core`

- Move task attempts, roster, workers, worktrees, leases, live control, and
  capability-routing logic behind dependency-clean interfaces.
- Keep blackboxd hosting `fleet-core` temporarily.
- Prove blackboxd server routes use only the public fleet-core surface.
- Add persistence migration readers before changing any on-disk shape.

Rollback: crate movement only; runtime authority remains blackboxd.

### Stage 4: start fleetd in shadow-view mode

- Run fleetd against an isolated socket and state directory.
- Feed it replicated read-only roster/events to verify view parity.
- Do not accept mutations or spawn workers in shadow mode.
- Compare roster, task lifecycle, transcript paths, and recovery outcomes.

This is observation duplication, not authority duplication.

Rollback: stop shadow fleetd.

### Stage 5: live authority cutover

- Make fleetd the single writer for task attempts, workers, worktrees, leases,
  roster, and live control.
- Point `bro-fleet-client` and the Fleet TUI at fleetd.
- Move low-level fleet control tools to fleetd.
- Keep temporary blackboxd control proxies only for compatibility clients, with
  explicit deprecation and no local fallback.
- Have workers reconnect from the temporary blackboxd endpoint to fleetd at a
  controlled session boundary, or drain older sessions before cutover.

Rollback requires draining new fleetd-owned attempts, then restoring the former
single authority. Never point two writers at the same task or lease store.

### Stage 6: blackopsd operational boundary

- Define an idempotent execution-request contract from blackopsd to fleetd.
- Adapt agents, workflows, atoms, schedules, mailboxes, and retries to use that
  contract instead of reaching task internals.
- Extract their durable owners into blackops-core and blackopsd.
- Move operational MCP tools from blackboxd to blackopsd.
- Publish definitions and outcomes into blackboxd as indexed records, never as
  the operational source of truth.

Rollback: host blackops-core in the transitional process while retaining the
same execution contract. Do not move operational state into fleetd.

### Stage 7: blackboxd capability boundary

- Serve corpus capabilities through a typed local endpoint.
- Have fleetd authorize and forward worker requests.
- Add reconnect, timeout, and fail-closed behavior for blackboxd outages.
- Move working-set LSP and local semantic operations into the worker.
- Remove in-memory daemon capability injection.

Rollback: fleetd can use a compatibility adapter while both services are on the
same release, but workers still talk only to fleetd.

### Stage 8: slim blackboxd

- Delete in-process harness launch and session-control sender maps.
- Remove `bro-harness`, `bro-tools`, provider, and V8 dependencies.
- Remove fleet task, roster, worker, worktree, live-control, agent, workflow,
  atom, schedule, mailbox, and operational-policy fields from blackboxd state.
- Remove compatibility control proxies after one supported migration window.
- Split build, packaging, and service restart procedures.

At this point independent build and restart are structural rather than
conventional.

## 5. Persistence and recovery

fleetd must persist enough to rebuild its authority without worker cooperation:

- task and session identity;
- worker identity, build, protocol version, and resume token hash;
- last acknowledged worker event sequence;
- worktree and lease ownership;
- last terminal or resumable status;
- transcript path and last indexed offset;
- pending idempotent commands and their outcomes where required.

Worker event logs remain the live session output source. fleetd stores compact
status and acknowledgement state, not a second complete transcript. blackboxd
ingests the logs as the durable searchable transcript corpus. blackopsd stores
logical operation transitions and mailbox cursors separately.

On startup fleetd loads durable state, opens the worker socket, accepts
reattachment, reconciles leases, replays event suffixes, and only then marks
unseen workers lost after a bounded grace period.

## 6. Surface migration

The final service surfaces are:

| Surface | Final service |
|---|---|
| `/control/*`, roster snapshot/stream, worker socket | fleetd |
| Low-level `bro_*` execution and control tools | fleetd |
| Agent, workflow, atom, schedule, and operational MCP tools | blackopsd |
| Corpus/search/knowledge/transcript/evidence MCP tools | blackboxd |
| provider and local working-set tools | bro-harness |

Clients that need corpus, operations, and execution tools should configure the
appropriate services. Temporary blackboxd proxies may preserve compatibility,
but each must be visibly remote, fail closed when its owner is absent, and have
a removal date.

## 7. Verification gates

Every stage must keep these gates green:

- `cargo +stable fmt --check`;
- `cargo nextest run --workspace` for the mid-cycle gate;
- `cargo nextest run --workspace --profile full` at closeout;
- no new compile edge from the contract bottom to an implementation crate;
- one authority per durable state cell;
- live same-host transcript tailing remains compatible;
- restart probes for the component introduced by that stage;
- explicit old/new protocol skew tests during migration windows.

The decisive live probes are:

1. replace `bro` while a turn streams;
2. replace the harness binary and show only a new session changes build;
3. restart fleetd and observe worker reconnect plus replay;
4. restart blackboxd and observe a live local turn survive;
5. restart blackopsd and observe live workers remain healthy;
6. kill one worker and observe sibling sessions remain healthy.

## 8. Risks and controls

- **fleetd becomes another monolith:** keep workflows, atoms, logical agents,
  mailboxes, schedules, and corpus implementations out of fleet-core.
- **blackopsd becomes a second blackboxd:** index operational records in
  blackboxd, but keep operational definitions and transitions authoritative in
  blackopsd.
- **false restartability:** require worker-initiated reconnect and replay before
  declaring the extraction complete.
- **dual authority during cutover:** shadow only read models; never dual-write a
  task, mailbox, operational transition, worktree, or lease state cell.
- **protocol churn:** version the handshake and evolve payloads additively within
  a supported window.
- **large capability payloads:** retain handles, previews, and bounded results.
- **lost working-set semantics:** place mutable-checkout LSP and refactor work in
  the worker, not behind a corpus RPC.
- **hidden compile coupling:** add dependency checks for each final binary.

## 9. Done definition

The extraction is finished when fleetd can restart and recover live workers,
blackopsd can restart and reconcile durable operations without killing those
workers, blackboxd can restart without changing either authority, blackboxd no
longer links operational or execution implementations, and the TUI reaches the
same live fleet through fleetd alone.

Further daemon splits require new evidence. Workflowd, atomd, or a remote
execution service are not implied by completing this plan.

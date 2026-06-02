---
title: "Fleet TUI — multi-instance coherent view (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "Desired UX and architecture for running multiple `bro fleet` TUIs on the same machine: every terminal sees the same roster, transcript, reports, queued turns, lifecycle state, and shared fleet actions. The design replaces per-TUI in-memory ownership with a single local fleet coordinator per fleet store, guarded by a lock, persisted through snapshots/event logs, and viewed/controlled by any number of TUI clients over local IPC. This preserves the no-blackboxd execution invariant while fixing split-brain live state."
---

# Fleet TUI — multi-instance coherent view (backlog)

## 1. Problem

`bro fleet` is now useful enough that an operator can plausibly open it from two
terminals at once: one large cockpit on a desktop monitor, one focused terminal
near an editor, or two panes pointed at different agents. The desired mental
model is simple:

> There is one fleet. Every `bro fleet` window is just a view/controller for that
> same fleet.

That is not the current model. The v1 cockpit deliberately runs daemon-free and
owns its orchestration in-process: each TUI constructs its own
`FleetOrchestrator`, `TaskStore`, tail broadcast channel, live `AgentHandle`s,
open child stdin handles, classifier companions, pending steer queues, roster
selection state, and other display state. Some data is persisted under the fleet
store and therefore visible after restart, but critical live details are
process-local:

- writable stdin handles for live bidirectional sessions;
- tail subscriptions and parsed transcript items as they stream;
- queued steers/pending user echoes;
- per-agent input history and recall cursors;
- report summaries, `needs_input`, and activity-derived fleet buckets before the
  next persisted snapshot;
- classifier companion handles and suggestions;
- stop/delete/resume intent and in-flight command feedback;
- any future mailbox delivery loop that needs live target handles.

A second `bro fleet` process therefore has partial, stale, or contradictory
knowledge. Worse, a naive reload path can interpret another process's live tasks
as orphaned/crashed because `TaskStore::load` was designed for cockpit restart,
not concurrent readers.

The multi-instance requirement is a product invariant, not a visual polish item:
when two TUIs are open on the same machine and same fleet store, they must show
the same fleet and make the same operations available.

## 2. Desired UX

### 2.1 Shared cockpit facts

All TUI instances connected to the same fleet store show the same values for:

- roster membership, display names, providers, models, project/cwd labels, costs,
  turns, and age/last-activity;
- fleet buckets: `Alerting`, `Waiting`, `Active`, `Idle`, `Interrupted`, and any
  future `Finished`/history bucket;
- current `report` message and `needs_input` bit;
- transcript contents, including operator steers, assistant text, thinking, tool
  calls/results, compact boundaries, turn footers, bounded-result riders, and
  classifier/intern activity;
- queued turns and interrupt/redirect state;
- per-agent input history, so a steer sent from one terminal is recallable from
  another;
- stop, resume, forget/delete, rename, compact, provider/config changes, and
  mailbox delivery outcomes.

Updates should feel live. A steer submitted in terminal A appears in terminal B's
single-agent transcript and roster teaser within one UI tick or one small IPC
latency budget, not only after a restart.

### 2.2 Local-only view state

Some state remains intentionally per terminal:

- selected roster row, current zoom pane, scroll offset, open modal, and help
  overlay;
- unsent composer draft text;
- input-history recall cursor (the history entries are shared, but the current
  cursor position is local);
- transient filter/search text;
- terminal dimensions and theme decisions.

Two operators may look at different agents without fighting each other's
selection. They only coordinate when they submit a fleet command.

### 2.3 Command semantics across terminals

Every mutating command is routed through the shared owner and is ordered in one
fleet-wide sequence:

- **Dispatch** from any terminal creates one roster row everywhere.
- **Steer** from any terminal appends a user turn with an origin label (at least a
  stable `client_id`; optionally a human terminal label) and queues behind any
  existing active turn according to the harness rules.
- **Interrupt** and **interrupt-and-redirect** are shared. If terminal A queues a
  steer and terminal B interrupts, the coordinator applies a deterministic order
  based on command sequence numbers and reports the outcome to both clients.
- **Rename** is last-writer-wins but visible as an event (`renamed by <client>`)
  so surprise is explainable.
- **Stop / forget** are destructive enough to require an acknowledgement when
  the target has pending input or another client has viewed/steered it recently.
  The acknowledgement is a UI affordance, not a lock on normal viewing.
- **Resume** of an `Interrupted` session restores one live handle in the shared
  owner. All connected TUIs see the row move out of `Interrupted`.

No TUI should silently perform a local-only mutation that another TUI cannot see.
If the shared owner rejects a command, every client can display the same error
reason.

### 2.4 Presence without collaboration theater

The cockpit is not a chat room, but light presence prevents operator confusion:

- footer/status line: `connected clients: 3`;
- optional row badge when another client is currently focused on that agent;
- command origin labels in transcript markers for operator turns when more than
  one client is connected.

Presence is diagnostic and should not block work.

## 3. Architecture decision: local fleet coordinator, not disk-only state

The correct architecture is a **single local fleet coordinator per fleet store**,
with every TUI instance acting as a view/controller over local IPC.

```text
terminal A ─┐
terminal B ─┼─ local IPC ── fleet coordinator ── child agents / stdin / tail
terminal C ─┘                    │
                                  ├─ TaskStore + task events
                                  ├─ fleet snapshot + event log
                                  ├─ classifier companions
                                  └─ mailbox / shared side loops
```

This is deliberately **not** a dependency on `blackboxd`. The coordinator can be
implemented in the `bro` binary (for example `bro fleet --serve` or an internal
spawn target) and links the same `blackbox` library code the current TUI links.
It is a small, store-scoped owner for live fleet state, not the global MCP daemon
and not an HTTP client of it.

### Why not only `flock` + JSON files?

A file lock and on-disk state are necessary, but not sufficient:

- they can elect one writer, but they cannot share open child stdin handles;
- they cannot broadcast live tail events without every TUI reimplementing a
  fragile file-follow protocol;
- they do not provide an ordered command plane for simultaneous steers,
  interrupts, resumes, and deletes;
- they make it too easy for a second TUI to load the store as if it were
  recovering after a crash, producing split-brain or false `Interrupted` rows.

A disk-only design would still need a live owner for session stdin and command
ordering. Once that owner exists, it should be explicit.

### Why not `blackboxd`?

The Fleet TUI design cluster intentionally keeps `blackboxd` out of the execution
path. Fleet sessions are top-level entrypoint agents, not daemon-managed bros;
the cockpit uses the orchestration core as a library and the harness as its
control protocol. Routing multi-instance state through the global daemon would:

- couple high-volume terminal UI lifecycle to the long-lived MCP service;
- blur the boundary between fleet-local store state and daemon task state;
- make development/restart flows riskier for active operators;
- contradict the existing fleet design's daemon-RPC-free invariant.

A local coordinator preserves the invariant while admitting that **in-process per
TUI** ownership was a v1 simplification, not the long-term state model.

## 4. Coordinator responsibilities

The coordinator is the only process that owns live fleet mutation for a store.
It owns:

- `FleetOrchestrator`, `TaskStore`, tail broadcast receiver/sender, and
  `AgentHandle` stdin handles;
- dispatch/resume/stop/forget/rename/compact/interrupt command execution;
- transcript parsing and durable transcript append;
- fleet-state derivation (`Active`, `Waiting`, `Alerting`, etc.);
- per-agent input history append and recall data;
- pending steer queues and replay reconciliation;
- classifier companion sessions and suggestion relay;
- mailbox delivery, once named-agent messaging lands;
- config reload/application for `fleet.json` changes that affect live sessions;
- snapshot + event-log persistence;
- client presence and heartbeat tracking.

The TUI owns rendering, local navigation, local composer drafts, and translating
keybindings/slash commands into coordinator commands.

## 5. Store layout

Use the existing `$BRO_HOME/fleet` as the fleet store root, adding explicit
runtime and state subtrees. Names are illustrative; the important invariant is
single-writer state with atomic files.

```text
$BRO_HOME/fleet/
  tasks/ or tasks.json             # existing TaskStore-compatible state
  events/                          # existing per-task event logs, if present
  state/
    fleet.snapshot.json            # last compact full snapshot for fast attach
    fleet.events.ndjson            # ordered fleet-level event log
    clients.json                   # optional persisted last-seen clients
  run/
    fleet.lock                     # advisory lock for coordinator election
    fleet.sock                     # Unix domain socket / local IPC endpoint
    fleet.pid                      # pid + generation + store hash
    fleet.heartbeat.json           # monotonic heartbeat for stale detection
  mailbox/                         # proposed named-agent mailbox substrate
```

Persistence rules:

1. The coordinator is the only writer for `state/` and live `TaskStore` files.
2. Files are written atomically (`tmp` + fsync + rename where practical).
3. Every mutating command gets a unique `command_id`; every accepted event gets a
   monotonically increasing `seq` and timestamp.
4. A snapshot records the highest included `seq`; clients attach by reading a
   snapshot then streaming events after that sequence.
5. A stale lock is broken only when the pid/generation is dead **and** the
   heartbeat is older than a conservative threshold.

## 6. IPC protocol shape

The protocol should be boring and local: newline-delimited JSON over a Unix
domain socket is enough. It mirrors the harness's event/control split but is
fleet-specific.

Client → coordinator commands:

```jsonc
{ "type": "hello", "client_id": "uuid", "version": 1, "supports": ["snapshot-v1"] }
{ "type": "subscribe", "after_seq": 1234 }
{ "type": "dispatch", "command_id": "uuid", "provider": "glm", "cwd": "/repo", "prompt": "..." }
{ "type": "steer", "command_id": "uuid", "agent_id": "...", "text": "..." }
{ "type": "interrupt", "command_id": "uuid", "agent_id": "..." }
{ "type": "compact", "command_id": "uuid", "agent_id": "..." }
{ "type": "rename", "command_id": "uuid", "agent_id": "...", "name": "parser audit" }
{ "type": "stop", "command_id": "uuid", "agent_id": "...", "ack": true }
{ "type": "forget", "command_id": "uuid", "agent_id": "...", "ack": true }
{ "type": "resume", "command_id": "uuid", "agent_id": "...", "text": "..." }
```

Coordinator → client messages:

```jsonc
{ "type": "snapshot", "seq": 1234, "agents": [/* roster rows + transcript heads */] }
{ "type": "event", "seq": 1235, "event": { "kind": "transcript_appended", "agent_id": "..." } }
{ "type": "command_ack", "command_id": "uuid", "seq": 1235 }
{ "type": "command_rejected", "command_id": "uuid", "reason": "agent_not_steerable" }
{ "type": "presence", "clients": [{ "client_id": "...", "focused_agent": "..." }] }
{ "type": "heartbeat", "coordinator_generation": "uuid" }
```

The first implementation can send full updated agent rows per event. Later
versions can use patches if payload size becomes visible.

## 7. Startup and attach flow

1. `bro fleet` resolves config, `BRO_HOME`, and the fleet store root.
2. It checks `run/fleet.sock` and `run/fleet.pid`.
3. If a healthy coordinator responds, the TUI attaches as a client.
4. If no coordinator responds, it tries to acquire `run/fleet.lock`.
5. The lock holder starts the coordinator for that store and waits for the socket
   heartbeat.
6. Other contenders back off and attach.
7. If a pid exists but is stale, the lock holder records a recovery event,
   starts a new coordinator, and only then applies `TaskStore` orphan recovery.

Important: a non-owning TUI must never call the current `TaskStore::load` path in
an orphan-recovery mode. Recovery is coordinator startup work after stale-owner
proof, not reader attach work.

## 8. Crash and recovery behavior

- **TUI crash/disconnect:** no agent is interrupted. The coordinator keeps owning
  child processes and state; remaining/new TUIs continue normally.
- **Coordinator crash:** connected TUIs show a reconnecting banner. If the
  coordinator cannot be restarted, live child stdin handles are lost and affected
  agents become `Interrupted`/recoverable on the next coordinator startup. The
  UI should explain `coordinator crashed; session must be resumed` rather than
  presenting a normal live row.
- **Machine sleep/wake:** heartbeats may pause. Use a stale threshold large enough
  to avoid split brain after wake.
- **Event-log corruption:** preserve the bad file, rebuild from the last valid
  snapshot + per-task persisted events when possible, and surface a red fleet
  diagnostic. Do not silently drop agents from the roster.
- **Version mismatch:** reject incompatible clients with a clear message and keep
  the coordinator alive for existing clients.

## 9. Security and scope

- Bind only to a filesystem socket in the user's `BRO_HOME`; never expose a TCP
  listener for the local coordinator.
- Create `run/` with user-only permissions where supported.
- Validate the store hash/generation on every attach so a stale socket path cannot
  control another store.
- Keep the coordinator's authority scoped to fleet actions. It should not become
  a generic daemon RPC surface or a replacement for `blackboxd`.
- Preserve existing multi-worktree safety: the coordinator may dispatch agents
  into isolated worktrees, but clients cannot make it mutate arbitrary peer files
  except through the normal agent/tool workflow.

## 10. Implementation plan

### Phase 1 — make concurrent readers safe

- Add a store lock/heartbeat concept and teach `TaskStore` fleet loading to
  distinguish **attach to live owner** from **recover stale owner**.
- Move input history, display rename, queued steer metadata, and report summary
  into persisted fleet-level state with sequence numbers.
- Add tests for two simulated `bro fleet` launches against one store: the second
  must not mark live tasks as crashed or mutate the store directly.

This phase reduces harm but does not deliver full multi-instance UX by itself.

### Phase 2 — coordinator process + local IPC

- Introduce a `FleetCoordinator` runtime that owns the existing
  `FleetOrchestrator` and exposes the IPC protocol.
- Add `bro fleet --serve <store>` or an internal subcommand used by `bro fleet`
  to spawn a detached coordinator.
- Convert the TUI to attach to the coordinator for snapshot, subscribe, and
  commands instead of constructing its own orchestrator directly.
- Keep an escape hatch for tests to instantiate the coordinator in-process.

### Phase 3 — shared live actions

- Route dispatch, steer, interrupt, compact, rename, stop, forget, and resume
  through the coordinator.
- Broadcast transcript and roster events to all clients.
- Add command acknowledgement/rejection UI.
- Add presence and multi-client command origin labels.

### Phase 4 — hardening and adjacent loops

- Move classifier companion lifecycle and mailbox delivery under the coordinator.
- Add event-log compaction and recovery tests.
- Add a diagnostics panel: coordinator pid/generation, connected clients, socket
  path, last persisted seq, and last heartbeat.
- Add ratatui snapshot previews for multi-client states: connected clients,
  remote steer arrival, rejected destructive action, coordinator reconnect.

## 11. Acceptance criteria

- Opening two `bro fleet` instances against the same `BRO_HOME/fleet` shows the
  same roster without marking the first instance's live agents as interrupted.
- Dispatching from either instance creates exactly one agent and both instances
  show it within one tick.
- Steering from either instance appends one operator turn, in the same transcript
  order, in both instances.
- `report(needs_input=true)` in an agent moves the row into `Waiting` in all
  attached instances.
- Rename/stop/forget/resume from one instance is visible in every other instance,
  with command rejection surfaced when applicable.
- Killing one TUI does not interrupt agents while another TUI or the coordinator
  remains alive.
- Killing the coordinator produces a clear reconnect/recovery path and never
  causes two live owners for the same store.
- The design remains daemon-RPC-free with respect to `blackboxd`; the only shared
  runtime is the fleet-local coordinator.

## 12. Non-goals

- Cross-machine fleet sharing. This design is same-user, same-machine, same fleet
  store.
- Collaborative editing of unsent composer drafts.
- Turning `blackboxd` into the fleet runtime owner.
- A full remote API for third-party clients. The IPC protocol exists to support
  local TUIs first.
- Solving provider quota/headroom routing; that remains in the provider-selector
  follow-on work.

## 13. Relationship

- Hub: [`fleet-tui.md`](./fleet-tui.md).
- As-built cockpit: [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md), especially
  §3 (in-process/no daemon), §5 (UX/state model), and §7 item 7
  (`FleetOrchestrator`).
- Existing follow-ons: [`backlog-follow-ons.md`](./backlog-follow-ons.md).
- Mailbox design whose delivery loop should move under the coordinator:
  [`backlog-named-agent-messaging.md`](./backlog-named-agent-messaging.md).
- Snapshot testing support for the new multi-client states:
  [`ratatui-snapshot-preview.md`](./ratatui-snapshot-preview.md).

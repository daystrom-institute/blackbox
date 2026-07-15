---
title: "Blackops service boundary: operational intent outside the flight recorder"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - orchestration
  - bro-harness
tags: [blackopsd, operations, agents, workflows, atoms, scheduling, service-boundary]
brief: "Extracts durable operational intent from blackboxd into blackopsd while keeping fleetd narrow: blackboxd records and retrieves what happened, blackopsd owns what should happen next, fleetd owns what is running now, and bro-harness owns how one session executes."
---

# Blackops service boundary: operational intent outside the flight recorder

## 0. Decision

Add `blackopsd` as a distinct operational authority between corpus and live
execution:

```text
blackboxd   what happened and what is known
blackopsd   what should happen next
fleetd      what is running now
bro-harness how one session executes
bro         how the operator sees and controls it
```

This gives the system four authority planes plus a view. It also keeps fleetd
from absorbing agents, workflows, atoms, schedules, ingress, and coordination
semantics merely because all of them eventually launch work.

The name is intentional. blackboxd returns to its flight-data-recorder
namesake: capture, retain, index, connect, and retrieve evidence. blackopsd owns
operational intent and automation. fleetd remains a deliberately narrow live
runtime supervisor.

## 1. Boundary test

Ask one question for every state cell or operation:

| Question | Owner |
|---|---|
| Is this a durable record of what happened or what is known? | blackboxd |
| Is this a durable statement of what should happen, when, and under whose policy? | blackopsd |
| Is this the current state of a running task, worker, worktree, or lease? | fleetd |
| Is this state needed to execute one model session against one working set? | bro-harness |
| Is this only terminal presentation state? | bro |

The split is semantic, not a crate-count exercise. A subsystem may publish an
indexed projection to blackboxd without making blackboxd its authority.

## 2. blackboxd: the FDR and corpus plane

blackboxd owns durable evidence and retrieval:

- transcript archive and transcript indexing;
- search, symbol, vector, embedding, and edge indexes;
- knowledge, notes, decisions, threads, roadmap, pins, and provenance;
- captured run events and searchable operational history;
- packet and artifact evidence intended for corpus retrieval;
- indexing generations and corpus semantic-status claims;
- corpus MCP tools and typed corpus capabilities.

The live worker remains the source of its current session event log, but
blackboxd ingests that log into the durable transcript corpus. This is the same
distinction as a flight recorder receiving sensor data: the producer owns the
live signal; blackboxd owns the retained, indexed record.

blackboxd does not decide what workflow fires, which agent runs, when a schedule
triggers, or how a worker is supervised.

## 3. blackopsd: the operational-intent plane

blackopsd owns durable automation and coordination semantics:

- atom definitions, versions, composition policy, and invocation intent;
- workflows, state machines, waits, schedules, crons, webhooks, and pollers;
- logical agent identities, parent/child graph, teams, roles, and mailboxes;
- reusable operational templates and brofile-level orchestration policy;
- approvals, operator-authority inputs, and policy-bound requested actions;
- whiteboards and shared coordination state;
- integration and publish intent for artifacts returning from workers;
- durable operation state independent of any one worker attempt;
- model-facing agent, workflow, atom, and operational MCP tools.

blackopsd asks fleetd to execute work. It does not spawn provider sessions,
manage worker sockets, tail provider streams, or own live worktrees. It consumes
fleet outcomes and decides the next operational transition.

Operational definitions may be indexed into blackboxd for discovery and
history. That index is a read projection, not the source of operational truth.

## 4. fleetd: the live execution plane

fleetd owns current execution mechanics:

- task and session attempts;
- worker process spawn, handshake, lease, reconnect, drain, and loss;
- provider/model/account allocation and concurrency;
- live worktree creation, seeding, ownership, and cleanup;
- roster, status projection, transcript path, and live control;
- steer, interrupt, resume, model change, compact, and cancel;
- execution admission and resource policy;
- low-level `bro_exec` and `bro_resume` style control surfaces.

fleetd receives an execution request from blackopsd or an authorized operator
client and returns a durable execution identity plus events/outcomes. It does
not interpret workflow graphs, atom composition, agent mailbox semantics, or
schedule recurrence.

## 5. bro-harness: the session plane

The worker owns provider and working-set execution:

- provider stream and agent loop;
- model context, compaction, and World State rendering;
- V8 code mode;
- local files, shell, Git, and working-copy LSP;
- session side state and event log;
- session-scoped RPC clients through fleetd.

The worker still has one service connection. fleetd routes operational calls to
blackopsd and corpus calls to blackboxd after applying the authenticated session
policy.

## 6. Agent ownership split

Agents span operational identity and live attempts:

| Concern | Owner |
|---|---|
| Canonical agent identity, path, role, parent edge | blackopsd |
| Mailbox, follow-up intent, logical status | blackopsd |
| Provider/model/worktree request policy | blackopsd |
| Concrete task/session attempt | fleetd |
| Worker process and live control | fleetd |
| Provider turn and context | bro-harness |
| Transcript archive and searchable history | blackboxd |

`spawn_agent` is therefore an operational transaction in blackopsd. blackopsd
creates or validates the logical child, then requests an execution attempt from
fleetd. A failed attempt does not erase the logical identity. A follow-up can
request another attempt against the same agent.

This separation makes cold resume and worker loss precise: blackopsd restores
identity and mailbox state; fleetd restores or replaces attempts; the worker
restores one session; blackboxd retains prior evidence.

## 7. Atom and workflow ownership split

Atoms and workflows also span definitions, executions, and records:

| Layer | Owner |
|---|---|
| Definition, version, input contract, composition policy | blackopsd |
| Scheduling and durable state-machine transition | blackopsd |
| Concrete execution attempt | fleetd |
| In-session implementation step | bro-harness or another worker |
| Transcript, metrics, trace, searchable artifact | blackboxd |

The current coupling between atom catalogs, workflow execution, and blackboxd
stores should be split along these layers. blackboxd may index atom and workflow
definitions supplied by blackopsd, but `atom_search` can distinguish the
authoritative operational catalog from corpus discovery results.

## 8. Service interactions

```text
operator / MCP client
  |- corpus query ------------------------------> blackboxd
  |- workflow, atom, agent, schedule ----------> blackopsd
  `- live task control ------------------------> fleetd

blackopsd
  |- request execution ------------------------> fleetd
  |- query knowledge/history ------------------> blackboxd
  `- publish definitions and outcomes ---------> blackboxd ingest

fleetd
  |- supervise session <-----------------------> bro-harness
  |- operational capability -------------------> blackopsd
  `- corpus capability ------------------------> blackboxd

bro-harness
  `- all remote capability calls --------------> fleetd broker
```

Cycles in the service graph do not imply compile cycles. Each call uses a typed
client over pure protocol contracts. A service never links another service's
implementation crate.

Avoid a synchronous RPC cycle for worker-originated operations. When a worker
calls `spawn_agent`, fleetd asks blackopsd for the logical transition. blackopsd
commits the child identity and returns a typed `RequestAttempt` effect. fleetd
ends that call, then executes the effect under the same operation ID. Scheduled
or externally-triggered operations use a durable blackopsd outbox to submit the
same idempotent execution request. blackopsd never calls back into a fleetd
request that is waiting on blackopsd.

## 9. Event and record flow

Each plane retains only the representation it owns:

- bro-harness appends the full ordered session event log;
- fleetd stores compact live task status, event acknowledgement, and transcript
  location;
- blackopsd stores durable operational transitions, logical identities,
  mailbox cursors, and requested versus observed outcomes;
- blackboxd ingests transcripts and operational records into searchable corpus
  generations.

Do not make one shared event database the synchronous write path for all four
services. Use bounded outboxes, idempotent consumers, stable event IDs, and
explicit projections. A blackboxd outage must not stop a live worker or lose the
event at its authoritative producer.

## 10. Restart matrix

| Restart | What continues | What pauses |
|---|---|---|
| blackboxd | workers, live tasks, workflows already represented in blackopsd | corpus queries and record ingestion catch-up |
| blackopsd | workers and fleet controls | new workflow/agent/atom decisions and mailbox operations |
| fleetd | blackopsd durable intent and blackboxd corpus | workers reconnect; live controls pause during recovery |
| one worker | all other services and workers | one execution attempt |
| bro client | everything | one operator view |

blackopsd restart recovery reloads durable operations and reconciles their
execution IDs against fleetd. It must not blindly re-dispatch an operation whose
outcome is merely unknown. Idempotency keys and requested/accepted/terminal
states are load-bearing.

## 11. Compile and protocol shape

Add operational contracts without contaminating the bottom:

```text
bro-core
  |- bro-protocol       fleet and worker lifecycle DTOs
  `- bro-capabilities   corpus, operations, agent, and execution traits

bro-rpc                 generic framing and typed adapters
blackops-core           operational state machines and policies
fleet-core              live execution state machines and supervision
blackbox corpus crates  records, stores, and indexes
```

`blackops-core` and `fleet-core` are siblings. Neither depends on the other.
Their daemons communicate through an execution contract implemented by an RPC
client. blackboxd does not link either core.

## 12. Extraction sequence

1. Define a narrow `ExecutionCapability` between operational intent and live
   task attempts.
2. Extract worker lifecycle and task control into fleet-core and fleetd.
3. Adapt existing agents, workflows, atoms, schedules, and mailboxes to request
   work only through that execution capability.
4. Extract those operational owners into blackops-core and blackopsd.
5. Publish operational definitions and outcomes into blackboxd as idempotent
   corpus records.
6. Move operational MCP surfaces from blackboxd to blackopsd.
7. Remove operational and execution state from blackboxd.

During migration, blackboxd may host fleet-core or blackops-core temporarily,
but each state cell has one owner. Shadow services may compare read models and
must not accept mutations.

## 13. Non-goals

- blackopsd is not a second transcript or search index.
- fleetd is not a workflow engine or agent database.
- blackboxd is not a scheduler merely because it indexes schedules.
- bro-harness is not allowed to bypass fleetd and address services directly.
- No generic event bus, workflowd, atomd, or mailboxd is added initially.
- Service separation does not change operator authority or broaden automation.

## 14. Acceptance criteria

The seam is real when:

1. blackopsd can restart without killing live fleet workers.
2. fleetd can restart without losing logical workflows, agents, or mailboxes.
3. blackboxd can restart without stopping either operational decisions already
   durable in blackopsd or live work in fleetd.
4. a workflow retry cannot duplicate an accepted fleet execution.
5. blackboxd indexes operational definitions without becoming their writer.
6. model-facing agent tools preserve identity across worker attempts.
7. each daemon builds without linking another daemon's implementation core.

## 15. Relationship

- [Process topology](process-topology.md) places all four authority planes.
- [Fleet extraction](fleet-extraction.md) extracts the narrow live-execution
  service that blackopsd calls.
- [Worker protocol](../bro-harness/worker-protocol.md) keeps the worker connected
  only to fleetd.
- [Agent runtime program](agent-runtime-program.md) orders fleetd, blackopsd,
  blackboxd slimming, and model-facing feature adoption.
- [Remote-worker boundary](../bro-harness/remote-worker-boundary.md) extends the
  execution plane to private filesystems and other machines.

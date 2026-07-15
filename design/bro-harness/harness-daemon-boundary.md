---
title: "The harness-daemon boundary: contract bottom and process separation"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - orchestration
  - surfaces
  - fleet-tui
  - daemon-runtime
updated: 2026-07-14
brief: "Revises the earlier in-process consolidation into a compiler-enforced and runtime-enforced boundary: bro-harness is a per-session worker, fleetd owns live execution, blackopsd owns operational intent, blackboxd owns FDR/corpus truth, and capabilities cross through session-scoped typed RPC."
---

# The harness-daemon boundary: contract bottom and process separation

> **Revision.** This document previously selected blackboxd as the in-process
> owner of all harness sessions, with an optional V8 companion. That process
> placement is superseded by the
> [four-plane process topology](../daemon-runtime/process-topology.md).
> The contract-bottom extraction, thin fleet client, capability inversion,
> API-native providers, and surface-governance work remain valid. Historical
> implementation detail is available in Git history rather than retained as a
> second current architecture inside this document.

## 0. Decision

`bro-harness` is a supervised per-session worker executable. It does not run
inside blackboxd or fleetd in production.

`fleetd` owns live execution, control, workers, and worktrees. `blackopsd` owns
agents, workflows, atoms, schedules, mailboxes, and durable operational intent.
`blackboxd` owns records, transcripts, indexes, and corpus services. The worker
connects only to fleetd. fleetd routes authorized operational and corpus calls
to their respective services.

All three share a pure contract bottom:

- `bro-core`: identifiers, references, common errors, and pure value types;
- `bro-protocol`: worker, fleet-control, roster, status, and transcript DTOs;
- `bro-capabilities`: capability traits and the DTOs in their signatures.

Transport I/O sits above those crates. Implementations never point back up from
the contract bottom.

## 1. As-built baseline

The current code has already completed important prerequisites:

- `bro-core`, `bro-protocol`, and `bro-capabilities` exist;
- `bro-harness` is both a library and an executable;
- API-native provider transports share one agent loop;
- daemon-backed corpus, atom, and refactor traits fail closed when absent;
- `bro-fleet-client` is a thin daemon client rather than a fleet owner;
- roster/status DTOs cross the control boundary;
- the Fleet TUI tails a same-host transcript path directly;
- surface policy and tool filters apply to harness dispatch;
- V8 code mode and local host-tool bindings are live.

The remaining architecture is transitional:

- blackboxd links `bro-harness` and `bro-tools`;
- blackboxd starts the harness loop as an in-process Tokio task;
- process-global slots hold installed daemon capability objects;
- an in-memory sender map owns live steering and interrupt control;
- task, fleet, corpus, and provider state coexist in one process.

The target removes those four runtime couplings without discarding the shipped
contract and model-facing work.

## 2. Boundary constitution

The following invariants replace the earlier consolidation rule:

1. `bro-harness` never depends on fleetd, blackopsd, or blackboxd implementation
   crates.
2. blackboxd never depends on `bro-harness`, `bro-tools`, provider transports,
   or the V8 runtime.
3. fleetd supervises the harness executable but does not link its implementation
   merely to avoid an RPC boundary.
4. The worker has one service relationship: its authenticated fleetd session.
5. fleetd may depend on blackopsd and blackboxd clients, but never on their
   implementation cores, stores, or server types.
6. blackopsd and blackboxd never link fleet-core merely to request execution.
7. Contract crates contain serde types and traits, not sockets, Tokio channels,
   filesystem handles, or service clients.
8. Capabilities are session-scoped values passed into harness construction.
   Process-global installation is forbidden.
9. Capability absence or outage fails closed with a typed cause.
10. Local tool authority remains enforced at call time inside the worker.
11. Restart and reconnect behavior is part of the contract, not an operational
    afterthought.

These constraints make both forbidden compile edges and forbidden runtime
backchannels visible.

## 3. Compile graph

```text
                       bro-core
                          ^
                    +-----+------+
                    |            |
             bro-protocol   bro-capabilities
                    ^            ^
                    |            |
                  bro-rpc -------+
                 /      \
                /        \
bro-harness worker   fleetd ---- operational client ---- blackopsd
                         `------ corpus client --------- blackboxd

bro CLI -> bro-fleet-client -> bro-protocol + bro-core
```

The implementation crates point down to contracts or sideways through an RPC
client. The contract crates point nowhere into an implementation.

Expected final binary dependencies:

| Binary | May link | Must not link |
|---|---|---|
| `bro` | fleet client, protocol, core, transcript parser | daemon implementations, fleet-core, blackops-core, harness, capabilities |
| `bro-harness` | tools, providers, V8, RPC, contract bottom | daemon implementations, fleet-core, blackops-core |
| `fleetd` | fleet-core, RPC, operational/corpus clients, contract bottom | harness implementation, blackops-core, corpus stores |
| `blackopsd` | blackops-core, execution/corpus clients, contract bottom | fleet-core, harness, corpus stores |
| `blackboxd` | corpus/index/storage, capability/ingest server, contract bottom | blackops-core, fleet-core, harness, tools, providers, V8 |

Provider and V8 changes therefore do not relink any daemon. Corpus changes do
not relink blackopsd, fleetd, or running worker code.

## 4. Session-scoped capability inversion

`bro-capabilities` remains the shared behavioral vocabulary. What changes is
how an implementation reaches the trait.

### Transitional in-process form

```text
blackboxd implementation -> global Arc<dyn Capability> -> harness tool
```

### Target form

```text
harness tool
  -> session-scoped RpcCapabilityClient
  -> authenticated worker connection
  -> fleetd policy and router
       |- local live-execution implementation
       |- typed blackopsd operational client
       `- typed blackboxd corpus client
```

The harness should receive one explicit session-services value containing its
available capabilities, local tool context, cancellation root, World State
inputs, and policy snapshot. Registration is derived from that value. Two
sessions may therefore have different authority without mutating a process
global.

The trait contracts remain transport-neutral. A unit test may pass an in-memory
fake; the production worker passes an RPC client. No trait signature contains a
socket, request ID, or transport retry policy.

## 5. Capability placement

| Capability class | Placement |
|---|---|
| File, shell, Git, web fetch, V8, working-copy LSP | Worker-local |
| Worktree, worker, live task attempt and control | fleetd |
| Agent spawn/message/wait, workflow, atom, schedule | blackopsd through fleetd |
| Corpus search, knowledge, transcripts, indexed hints | blackboxd through fleetd |
| Mixed refactor operations | Split: corpus planning where required, working-copy validation/apply in worker |

The worker does not receive a blackboxd URL or corpus credential. fleetd binds
every forwarded call to the authenticated session and its policy envelope.

Large results use handles, previews, and bounded materialization. The RPC seam
must not regress the context-economy work by copying large corpus or refactor
payloads through multiple model-visible layers.

## 6. Tool binding and MCP

The previous in-process design tried to remove self-MCP by turning blackboxd
tools into direct function calls. The process split changes the mechanism, not
the policy goals.

- Worker-local tools remain direct Rust calls.
- Live fleet operations use typed worker RPC, not an MCP loopback.
- Agent, workflow, and atom operations route through fleetd to blackopsd.
- Corpus capabilities use typed fleetd-to-blackboxd RPC.
- Genuinely external MCP servers remain MCP connections.
- External clients use fleetd MCP for execution tools, blackopsd MCP for
  operational tools, and blackboxd MCP for corpus tools.

Surface evaluation still decides which tools are admitted and callable. Direct,
code-mode, RPC-backed, and MCP-backed projections must share the same effective
policy. Hiding a definition without denying the call path remains insufficient.

The worker protocol is not a general MCP transport. It has session identity,
commands, event replay, leases, and typed capability calls that MCP does not
provide as one coherent lifecycle.

## 7. Worker process contract

The harness executable owns one session. It receives explicit configuration,
never process-global identity mutation:

- task and session IDs;
- fleet socket and bootstrap credential;
- provider, model, auth reference, and endpoint;
- working directory and worktree identity;
- session event-log and side-state locations;
- initial tool/capability policy;
- resume inputs when applicable.

It connects outbound and implements the
[worker protocol](worker-protocol.md): handshake, version negotiation,
sequenced events, acknowledgements, idempotent commands, capability calls,
heartbeats, leases, drain, reconnect, and replay.

The current Claude stream-JSON envelope may remain an external compatibility
format and transcript parser input. It is not sufficient as the internal worker
control contract because it lacks bidirectional lifecycle and recovery
semantics.

## 8. V8 and shell failure domains

V8 remains inside bro-harness. The harness worker process is its failure domain.
The former optional `bro-code-mode-host` companion is removed from the initial
plan because it would add another protocol and process without further
protecting any daemon.

The in-worker V8 embedding still requires:

- heap and execution limits;
- cross-thread termination;
- panic containment at every Rust-to-V8 callback boundary;
- denied ambient globals;
- bounded egress;
- the cell lifecycle invariants in
  [Code-mode runtime lifecycle](code-mode-runtime-lifecycle.md).

Shell tools remain worker child processes with timeout, cancellation, environment
scrubbing, and resource caps. A worker boundary protects peer sessions and both
daemons; it does not make arbitrary shell execution safe inside that worker's
worktree.

## 9. Fleet client boundary

`bro` remains a thin client. It owns no task process, harness channel, worktree,
mailbox, or authoritative roster state. It talks to fleetd for commands and
snapshots and tails same-host transcript files for full output.

The fleet client continues depending only on `bro-protocol` and `bro-core` plus
client-side view logic. It never links `bro-capabilities`, fleet-core,
bro-harness, or blackboxd.

This separation makes TUI rebuilds and crashes operationally irrelevant to live
work.

## 10. Restart behavior

- Worker restart affects one session and uses balanced persisted history for
  resume.
- Harness binary replacement affects new workers only.
- fleetd restart preserves workers through outbound reconnect and event replay.
- blackopsd restart preserves workers; operational and mailbox calls pause until
  it reconciles durable intent with fleet attempts.
- blackboxd restart preserves workers and operations; corpus calls and record
  ingestion resume when it returns.
- `bro` restart reconstructs its view from fleetd and transcript paths.

The detailed matrix lives in
[Process topology](../daemon-runtime/process-topology.md). The wire mechanics
live in [Worker protocol](worker-protocol.md).

## 11. Local and remote workers

The first worker is same-host and normally shares a worktree path with fleetd.
That is a process and restart boundary, not yet a filesystem or network trust
boundary.

[Remote-worker boundary](remote-worker-boundary.md) remains the later rung where
the worker has a private filesystem or another machine. At that point artifact
transfer, integration, encryption, host identity, and working-set truth become
additional protocol concerns. The local protocol should not pretend those costs
exist, but its IDs and authority model must not make remote execution impossible.

## 12. Migration

The strangler sequence is:

1. make harness services explicit and session-scoped;
2. launch bro-harness as a child while blackboxd retains authority;
3. add worker RPC, reconnect, replay, and leases inside blackboxd;
4. extract dependency-clean fleet-core;
5. launch fleetd in read-only shadow mode;
6. cut live execution authority to fleetd;
7. extract operational authority into blackopsd over a typed fleet execution
   contract;
8. serve corpus capabilities and record ingest from blackboxd;
9. remove operational and execution dependencies from blackboxd.

See [Fleet extraction](../daemon-runtime/fleet-extraction.md) for rollback and
verification at each step.

## 13. Verification contract

Tests must prove:

- forbidden crate edges remain absent;
- two sessions can use different capability sets concurrently;
- standalone worker startup without a valid fleet handshake fails closed;
- a worker cannot address another session through capability payloads;
- direct and code-mode tools share call-time policy;
- fleetd restart causes reconnect and exact event replay;
- blackopsd restart preserves live workers and logical agent identity;
- blackboxd outage does not terminate a local-only turn;
- one worker crash leaves siblings and all daemons healthy;
- old and new supported worker builds coexist during rolling replacement;
- same-host transcript tailing survives worker RPC loss;
- unsupported protocol versions produce a precise failure.

## 14. Relationship

- [Process topology](../daemon-runtime/process-topology.md) owns service
  authority and restart outcomes.
- [Blackops service boundary](../daemon-runtime/blackops-service-boundary.md)
  owns the operational-intent plane.
- [Fleet extraction](../daemon-runtime/fleet-extraction.md) owns migration.
- [Worker protocol](worker-protocol.md) owns the same-host session RPC.
- [Agent runtime program](../daemon-runtime/agent-runtime-program.md) orders this
  boundary with the Codex-inspired feature work.
- [Code-mode runtime lifecycle](code-mode-runtime-lifecycle.md) owns cell state
  inside the worker.
- [Model-facing agent capability](model-facing-agent-capability.md) applies the
  session-scoped capability pattern to collaboration.
- [Concurrency model](../daemon-runtime/concurrency-model.md) remains binding
  within blackboxd, blackopsd, fleetd, and each worker.

---
title: "Codex mainline adoption map for bro-harness"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - code-mode
  - orchestration
brief: "Promotes the 2026-07-14 Codex source refresh onto the new service topology. First harden V8 cell semantics and restore bro-harness as a per-session worker, then extract fleetd and blackopsd, generalize model-visible state, and expose blackopsd-owned logical agents backed by fleetd attempts."
---

# Codex mainline adoption map for bro-harness

## 0. Decision

The [Codex main@8aae858958 research snapshot](../../research/harness/codex/codex-main-8aae858958.md)
contains three feature changes worth promoting into first-class bro-harness
designs, plus one Blackbox-specific process correction needed to host them:

1. [Code-mode runtime lifecycle](code-mode-runtime-lifecycle.md): make the
   existing V8 surface race-safe before extending its DSL.
2. [Model-visible World State](model-visible-world-state.md): generalize the
   existing environment reference item and dispatch emission baselines into a
   typed, persisted model-knowledge ledger.
3. [Model-facing agent capability](model-facing-agent-capability.md): let the
   model collaborate with blackopsd-owned logical agents backed by fleetd
   attempts, without importing either scheduler into the harness.
4. [Process topology](../daemon-runtime/process-topology.md): run bro-harness as
   a per-session worker so provider and V8 changes do not share blackboxd's
   build, restart, or failure domain.

The implementation order is deliberate. Cell correctness and explicit
session-scoped dependencies land before workerization. Worker reconnect and
fleetd extraction land before native agents. World State then gives changing
agent, tool, skill, and service availability a restorable model-facing form.
The complete dependency order lives in the
[agent runtime program](../daemon-runtime/agent-runtime-program.md).

## 1. Current bro-harness baseline

bro-harness already has more of the Codex shape than the June comparison did:

- `crates/bro-code-mode` ships `exec`/`wait`, nested admitted tools,
  cross-cell storage, and local function/namespace additions.
- `ContextualUserFragment`, `TurnContextItem`, environment deltas, and
  `reference_context_item` implement a narrow context-diff baseline.
- dispatch scope and pins persist separate last-emitted baselines.
- compaction is model-keyed, proactive, provider-aware, and persisted.
- session IDs already feed Responses `prompt_cache_key`.
- MCP tools already share one registry and deferred `tool_search` surface.
- `bro-capabilities` already carries fail-closed capabilities, currently
  installed in-process and targeted to become session-scoped RPC clients.

The adoption target is therefore not "copy Codex." It is to strengthen the
existing seams where the new source provides a tested invariant.

## 2. Adoption matrix

| Codex delta | Bro-harness decision | Priority | Owning design |
|---|---|---:|---|
| Per-cell actor, linearized terminal state, hierarchical cancellation | Adopt while preserving local code-mode additions | P0 | [runtime lifecycle](code-mode-runtime-lifecycle.md) |
| Process-owned code-mode host | Do not add separately; the per-session harness worker is the V8 process boundary | skip | [runtime lifecycle](code-mode-runtime-lifecycle.md) |
| Reattachable session worker | Adopt as Blackbox's process-containment and rolling-upgrade boundary | P0 | [worker protocol](worker-protocol.md) |
| JIT-less V8 | Add as an opt-in hardening/compatibility mode | P2 | [runtime lifecycle](code-mode-runtime-lifecycle.md) |
| Persisted World State | Generalize current context baselines | P1 | [World State](model-visible-world-state.md) |
| Multi-agent v2 lifecycle | Adopt through `bro-capabilities`; blackopsd identity plus fleetd attempts | P1 | [agent capability](model-facing-agent-capability.md) |
| Context remaining tool | Adopt after World State plumbing; read-only and small | P1 | compaction follow-on |
| Explicit new context tool and lineage | Adopt after compaction reconstruction has stable window identity | P2 | compaction follow-on |
| MCP stdio catalog cache | Adopt sanitized definition reuse; calls remain live-only | P2 | `backlog-transport-polish.md` follow-on |
| Shadow lexical skill selection | Copy the experiment discipline, not the whole Codex skills subsystem | experiment | skill/lens projection follow-on |
| Extension contributor framework | Use only small typed seams demanded by the three designs | defer | none |
| Current time and interruptible sleep | Existing context/wait surfaces are sufficient for now | defer | none |
| Deferred execution-environment wait tool | No matching bro-harness environment lifecycle yet | skip | none |
| Generated-image helper | No deliberate image-generation harness capability yet | skip | none |

## 3. Delivery sequence

### Stage A: contracts, cell correctness, and explicit dependencies

- Freeze the process ownership and worker restart matrix.
- Introduce one actor owner per cell.
- Port completion/termination/store-commit invariants and regression tests.
- Add authoritative session shutdown and dropped-observer behavior.
- Replace process-global capability slots with explicit session services.
- Keep current process ownership until these tests pass.

This stage changes internals, not model-facing tool schemas.

### Stage B: workerization and service extraction

- Launch one bro-harness process per selected session under the current owner.
- Add worker reconnect, sequenced event replay, commands, leases, and
  session-scoped capability RPC.
- Extract fleet-core and cut live authority to fleetd.
- Extract agents, workflows, atoms, schedules, and operational intent to
  blackopsd over the fleet execution contract.
- Serve corpus capabilities from blackboxd through fleetd.
- Remove harness, provider, tool, and V8 dependencies from blackboxd.

### Stage C: model-visible state

- Replace the monolithic `reference_context_item` with section snapshots while
  preserving the current provider-specific composition strategy.
- Migrate environment first, then dispatch scope/pins, then AGENTS/project
  instructions and tool-manifest state.
- Reconcile retained fragments after resume and compaction.

### Stage D: native agents

- Add session-bound agent DTOs and a trait to `bro-capabilities`.
- Implement logical identity and mailboxes in blackopsd, concrete attempts in
  fleetd, and the trait in the worker with a session-scoped RPC client.
- Register model-facing tools only when the capability and collaboration policy
  are present.
- Persist canonical identity and mailbox events before adding cold resume.

### Stage E: smaller follow-ons

- Add `get_context_remaining`, then explicit new-window lineage.
- Add sanitized stdio MCP catalog reuse if startup measurements justify it.
- Consider worker-local JIT-less V8 after compatibility measurements.

### Stage F: disclosure experiments

- Run a bounded deterministic selector in shadow mode over whatever skill,
  system-memory, or lens catalog the fleet/corpus services project.
- Measure selection rank against actual load/invocation.
- Change prompt disclosure only after the false-negative envelope is known.

## 4. Guardrails

- Preserve bro-code-mode's local function store and namespace work. Upstream
  refresh is a semantic port, not a directory replacement.
- The harness never depends on fleetd or blackboxd implementation crates. Agent
  control crosses through bottom-contract traits implemented by session RPC.
- The harness worker is the V8 failure boundary. Do not add a companion process
  until evidence shows a second boundary inside one session is worth its cost.
- World State describes what the model is expected to know. It is not a generic
  application database or release ledger.
- Agent messages do not broaden authority. Worktree creation, model override,
  service tier, and external side effects remain controlled by blackops/fleet
  policy and operator inputs.
- Cached MCP definitions never authorize calls or carry cached approvals.

## 5. Relationship

- [Codexification](codexification.md) owns the original loop/context convergence
  plan. World State is its current successor for mutable context tracking.
- [The cell DSL](code-mode-cell-dsl.md) owns composable values, namespaces, and
  tenant semantics. Runtime lifecycle owns execution correctness beneath it.
- [Harness-daemon boundary](harness-daemon-boundary.md) owns the compile DAG and
  capability inversion. The agent design extends that pattern.
- [Agent runtime program](../daemon-runtime/agent-runtime-program.md) connects
  this feature map to workerization, fleet extraction, and blackboxd slimming.
- [Remote-worker boundary](remote-worker-boundary.md) remains the off-host
  placement design. The initial worker is same-host and does not yet create a
  private-filesystem or hostile-network boundary.

---
title: "Bro-Harness"
kind: design-hub
corpus: blackbox-design
topic:
  - bro-harness
brief: "Nav hub for the bro-harness design cluster: the custom headless coding-agent worker that speaks provider APIs directly behind one Transport, runs its own tool and V8 loops, records a session event log, and connects to fleetd through a typed worker protocol."
---

# Bro-Harness

`bro-harness` (`crates/bro-harness`, `crates/bro-tools`) is the custom headless
coding agent that speaks provider APIs directly behind one `Transport` interface,
runs its own tool-calling and V8 code-mode loops, and records the session event
stream. The target production unit is one supervised worker process per session.
It connects to fleetd for live control and authorized remote capabilities;
fleetd routes operational calls to blackopsd and corpus calls to blackboxd.
Provider, context, local tools, working-copy LSP, and V8 state remain local.
See `PROJECT.md` under "Provider & Agent Surfaces" for routing facts.

**Top-level abstraction, not an orchestration sub-topic.** By invariant the
harness never depends on fleetd or blackboxd implementation crates. Its one
production service relationship is an authenticated, session-scoped RPC to
fleetd. The shared contract bottom and `bro-rpc` keep this runtime dependency
typed without introducing an implementation cycle.

This page is the **nav waypoint** — start here, then follow a link. Per-feature
detail lives in each linked doc; this hub keeps only the sort.

## Shipped (as-built records)

The built core. Each is an `archived` as-built record; residual work, where any,
points to a backlog doc.

- [Custom provider harness](anthropic-harness.md) — three transports, agent loop,
  SSE streaming, model-keyed compaction, bidirectional session/control protocol,
  deferred tiering, recursion guard.
- [Tool surface](bro-harness-tool-surface.md) — the built-in subset: shell
  quartet, `file_read`/`content_search`/`glob`, `todo_write`.
- [Clipboard (`clip_*` registers)](bro-harness-clipboard.md) — the nine-tool
  settled-ref register store on the `side` spine.
- [Tool chaining (the ref ABI)](bro-harness-tool-chaining.md) — Stages 1–2:
  settled refs + `kind`-tagged producers/consumers.
- [Hooks & nudges](bro-harness-hooks.md) — system-prompt split, hook seam,
  delivery, Nudger v1 + four rules.
- [Diagnostics (window-0)](bro-harness-diagnostics.md) — the instant/error-tier
  MVP (`bro-lsp` + per-mutation rider); upper tiers deferred.
- [Tool-arg defaulting](tool-arg-defaulting.md) — proposed: host-set
  default/pin table (`additional_context`) that fills or enforces tool args
  the model elides; cwd param conformity + dispatch hardening (gap-16d79781).
- [Search provider abstraction](search-provider-abstraction.md) — proposed:
  replace the bare `web_search: bool` (default-ON, ungoverned, three divergent
  per-transport spellings, vibe-bh hole) with a normalized `SearchConfig` + two
  axes (native emission shape vs hosted backend) folded back through the
  `ToolFilter`. Subsumes the backlog web_search-fallback + result-normalization
  bullets.
- [Remote-worker boundary](remote-worker-boundary.md) — proposed: what
  irreducibly stays in blackboxd and fleetd when a harness worker gets a private
  container or machine. Working-set versus corpus truth remains the placement
  function; isolation concentrates governance at fleet dispatch and artifact
  re-entry.
- [Refactor tools v2: the in-box DSL](refactor-tools-v2.md) — proposed: dissolve
  the 100+-kind `bbox-refactor` catalog into code-mode cell programs over a
  small binding algebra (facts `code.*`/`lsp.*`/`analysis.*`, EditSet algebra,
  one `apply()` choke point). Adjudication inverted to exception handler
  (apply bounces with structured findings); catalog inverts into a script
  library; lineage-computed `semantic_status` on the artifact. Migration is a
  harness-first strangler ending **in-harness only** (decided): external/MCP
  agents route via `bro_exec`/`bro_resume` or canned atoms; RX-V2 retires with
  the MCP surface.
- [The cell DSL: composable in-box infrastructure](code-mode-cell-dsl.md) —
  proposed: the platform layer under refactor-tools-v2 and later in-box
  domains. Values-not-refs (salvaged from narf-data-model, validated by the
  codex-native fallback); the hash-anchored Span as the composability quantum;
  provenance via a host-side issuance ledger (weakest-link tiers computable
  without taint-tracking or cell-supplied tags); the namespace contract for
  shipping a domain (bindings + hand-authored TS declarations + tiers +
  optional choke point, composed at dispatch); sessions/batching/no durable
  promises in-box. Tenant test: refactor + diagnostics ship as namespaces with
  zero runtime changes.
- [Codex mainline adoption map](codex-mainline-adoption.md) - proposed ordering
  for the July 2026 source refresh: code-mode correctness, model-visible state,
  native agent capability, then smaller context/MCP/disclosure follow-ons.
- [Code-mode runtime lifecycle](code-mode-runtime-lifecycle.md) - proposed cell
  actor, terminal-state, cancellation, observation, and in-worker V8 lifecycle
  beneath the existing `exec`/`wait` surface.
- [Worker protocol](worker-protocol.md) - proposed handshake, authenticated
  reconnect, event replay, idempotent controls, leases, and session-scoped
  capability RPC between bro-harness and fleetd.
- [Model-visible World State](model-visible-world-state.md) - proposed typed,
  persisted generalization of `reference_context_item` and dispatch emission
  baselines, including retained-fragment repair.
- [Model-facing agent capability](model-facing-agent-capability.md) - proposed
  spawn/message/followup/interrupt/list/wait surface backed by a fail-closed
  capability: blackopsd owns logical identity/mailboxes and fleetd owns concrete
  execution attempts.
- [Blackops service boundary](../daemon-runtime/blackops-service-boundary.md) -
  proposed operational plane for agents, workflows, atoms, schedules, and
  integration intent, separate from fleetd's live execution plane.
- [Agent runtime program](../daemon-runtime/agent-runtime-program.md) - the
  implementation spine joining the Codex refresh, workerization, fleetd,
  blackboxd slimming, World State, and native agents.

## Backlog (proposed — pick this up)

- [Transport & tool polish](backlog-transport-polish.md) — MCP connection
  pooling, `codex_auth` retry wrapping, deferred-manifest trimming, web_search
  fallback, structured output, RTK output compaction (← thread-ca160aa2 item-5 +
  anthropic open-questions).
- [Tool chaining Stage 3](backlog-tool-chaining-stage-3.md) — pending refs =
  Task; gated on an async producer existing.
- [Hooks catalog-metadata channel](backlog-hooks-catalog-metadata.md) — v2
  rule-source; gated on the adoption loop.
- [Diagnostics check & truth tiers](backlog-diagnostics-truth-tiers.md) — flycheck
  lints + orchestrator-owned truth tier + the `bro-lsp`/`src/lsp` fork.
- [Per-call privilege escalation](backlog-per-call-escalation.md) — Codex-style
  escalate+justification; gated on unifying the privilege model.
- [Neuralyze (rewind + carry a message)](bro-harness-neuralyze.md) — fully
  unbuilt: checkpoint substrate, context rewind, file inverse-diff journal.
- [NARF capability library and prepared scripts](narf-capability-library.md) —
  proposed authoring-layer middle tier: session-local helpers, decay-managed
  reusable functions, capability scout, and prepare-before-run script refs.
- [NARF data model: one durable KV, values not refs](narf-data-model.md) —
  proposed: collapse the two half-built value systems (the `RefState` ref
  substrate + the clipboard/`clip_*` chaining ABI) into ONE durable session KV.
  A cell is out-of-context, so tools return values, JS transforms values the cell
  holds, and the context discipline bounds the cell's *return*. KV surface is
  box-edge-split on "the box never selects": in-box is exact-deref-by-known-name
  (set/get/peek/delete, **no enumeration**); `list`/`keys` (discovery/selection)
  are out-box. **Supersedes** [`narf-tool-placement.md`](narf-tool-placement.md)
  (now archived) + the §9-1 ref substrate.
- [Workflow-JS: composable state machines as the workflow surface](narf-workflow-js.md)
  — proposed: finishes the [typed-cells](narf-typed-cells.md) durable tier. The JSON
  node graph is already a state machine, so replace it with a state-machine *library*
  in the sandbox (Stateless/ZCrew.StateCraft-shaped core: states, transitions fired by
  triggers, guards, entry/exit/action handlers) plus NARF-native child machines,
  reusable at every scope (the verb sets the tier). The machine STRUCTURE is data the
  daemon *validates* (transitions target declared states, trigger alphabet closed after
  registration, reachable/`.terminal()` states, child machines resolve, child contracts
  typecheck) and renders to mermaid; the BODIES are JS at shell trust (validation =
  well-formedness, not safety). Composition lifts the hand-composed StateCraft
  OrderProcessor pattern into a first-class primitive (parent owns N children, commands
  down via actions, joins via WhenAll/WhenAny + guards on child state, children signal
  up) — subworkflow/ensemble/foreach/fork all this.
  Durability = a tree of independently-persisted `{state, KV}`; the daemon backs the
  durable primitives, pure primitives stay JS. Agents learn it via tooldocs + signposts
  + a `sm-state-machines` memory. `effects` elided (supersedes typed-cells §1.2).
  Closes typed-cells §7 items 2/3.

## Cluster conventions

- The per-session async/temporal layer is harness-owned. Live tasks, workers,
  worktrees, and leases are fleetd-owned. Logical agents, mailboxes, workflows,
  and atoms are blackopsd-owned. None is hidden behind a synchronous MCP
  fiction.
- The harness has a typed runtime dependency on fleetd through the worker
  protocol, never a compile dependency on fleetd or blackboxd implementations.
- Working-copy LSP and local tools live in the worker. Corpus capabilities route
  through fleetd and fail closed when unavailable.
- The `side` persistence spine is the keystone — clipboard, nudge ledger, todos,
  and (future) neuralyze checkpoints all ride it; nothing stateful needs new
  persistence machinery.
- Privilege lives in `SafetyPolicy` + the brofile allow/deny layer. Nudges steer,
  they never gate; neuralyze rewinds, it never escalates privilege.
- Harness state is session-scoped. Cross-session coordination happens through
  fleetd-brokered contracts whose operational authority lives in blackopsd.
- Provider-agnostic ambient text uses **bare** tool names (`bbox_note`, not
  `mcp__blackbox__bbox_note`); FQDN surfacing is a per-CLI concern.

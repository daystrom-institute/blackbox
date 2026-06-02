---
title: "Bro-Harness Agent Loop — Spec"
kind: spec
corpus: blackbox-spec
domain: bro-harness
spec: agent-loop
topic:
  - specs
  - bro-harness
  - agent-loop
status: draft
sources:
  - "research:research/harness/agent-loop.md"
  - "research:research/harness/session-lifecycle.md"
  - "vendor:Anthropic Messages API (stream events, stop_reason, pause_turn)"
  - "vendor:OpenAI Responses API"
supersedes: null
last_reviewed: "2026-06-02"
---

# Bro-Harness Agent Loop — Spec

> **STATUS: `draft` stub.** Skeleton + source pointers only; clauses are not yet
> mined. Pick this up per the backfill shape in
> [the domain charter](bro-harness-spec.md). The canon below is the *frontier*,
> not a finished contract.

The normative contract for the core turn loop of `crates/bro-harness` — what the
harness must do between receiving a user turn and yielding control, across every
backend transport.

## Scope

The turn loop only: turn boundaries, stop/`end_turn` detection, parallel tool
calls, tool-result threading, operator steering mid-flight, interrupt handling,
`pause_turn`/resume, and the recursion guard. Wire-level concerns (retry,
backoff, SSE, envelope) belong to [transports.md](transports.md); window
shrinking belongs to a future compaction spec.

## Clauses to specify

Each becomes an atomic, tier-tagged clause during mining. Placeholders now:

- **Turn boundary & stop detection** — when a turn is complete; `stop_reason` /
  `end_turn` handling per transport; spurious-stop detection. `[vendor]` `[research]`
- **Parallel tool calls** — multiple tool_use blocks in one turn; ordering and
  result threading back into the next request. `[vendor]` `[research]`
- **Tool-result threading** — how results are appended; role alternation;
  server-tool-block preservation (cf. commit `efc82bf`). `[vendor]`
- **Operator steering mid-flight** — stdin NDJSON user turns queued while a turn
  is active, applied at the next model-call / turn boundary. `[research]` `[derived]`
- **Interrupt** — interrupt semantics at a turn boundary; what state survives. `[derived]`
- **`pause_turn` / resume** — pause_turn detection and turn continuation (Anthropic;
  cf. commit `efc82bf`). `[vendor]`
- **Recursion guard** — mechanical guard on recursive `bro_*` orchestration tools
  for dispatch-capable providers; `bro_report` exempt; `allow_recursion` bypass. `[derived]`

## Conformance (to be wired)

| Clause | code anchor | intent (design) | evidence (research) |
|--------|-------------|-----------------|---------------------|
| _stub_ | `crates/bro-harness/src/agent_loop.rs` | `design/bro-harness/brodex-agent-loop-learnings.md` | `research/harness/agent-loop.md` |

Canonicalization commits to mine for rationale: `efc82bf` (server-tool blocks +
pause_turn resume), and the agent-loop sections of `design/bro-harness/anthropic-harness.md`.

## Open

<!-- TODO(spec): mine crates/bro-harness/src/agent_loop.rs (75.9K) for the as-built
     loop; reconcile against research/harness/agent-loop.md and the Codex-lens
     extensions; convert each clause above into a sourced normative statement;
     complete the Conformance table; flag any code↔canon divergence as a gap. -->
- Does steering apply only at turn boundaries or also at intra-turn model-call
  boundaries? (research/harness/agent-loop.md notes the latter for Claude Code.)
- Recursion-guard exact tool set + the `allow_recursion` plumbing
  (`src/tools/dispatch.rs`).

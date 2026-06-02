---
title: "Claude · Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: agent-loop
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: mixed
topic:
  - harness
  - claude
  - agent-loop
brief: "Claude Code 2.1.160's agent loop: parallel tool execution for independent calls (the harness explicitly instructs batching tool_use blocks into one turn), stop-reason classification including pause_turn resume, and interrupt handling via role-alternation repair + tool-result padding. Parallel-tool guidance and the dependency rule are observed first-hand; stop-reason internals and the precise interrupt/steering merge semantics are partly from the api-robustness mine, partly open."
---

# Claude · Agent Loop

> **Provenance.** Parallel-tool guidance and the dependency rule: direct
> observation this session — **high**. Stop-reason handling, `pause_turn` resume,
> and interrupt repair: `design/bro-harness/bro-harness-api-robustness.md` /
> `brodex-agent-loop-learnings.md` (mined + cross-transport) — **mixed**. Internal
> loop call graph is **open**. See [snapshot](claude-2.1.160.md).

See the axis: [Agent Loop](../agent-loop.md).

## Parallel tool calls (high)

The harness instructs the agent to batch independent tool calls into a single
turn: *"If you intend to call multiple tools and there are no dependencies
between the calls, make all of the independent calls in the same block,
otherwise you MUST wait for previous calls to finish first to determine the
dependent values."* So parallelism is **agent-directed** (the model decides which
calls are independent), executed concurrently by the loop.

## Stop detection & resume (mixed)

- Stop reasons classified: `end_turn`, `tool_use`, `max_tokens`, `pause_turn`.
- **`pause_turn`** (server tool hit its iteration limit) is **resumed**, not
  mapped to a generic stop — see [claude-robustness](claude-robustness.md).
- **Spurious-stop detection** flags empty-output / outstanding-async turn ends.

## Interrupts & steering (mixed)

On interrupt, the loop performs **role-alternation repair** and **tool-result
padding** so a half-finished dispatch never orphans a `tool_use`. The precise
merge semantics of an operator steering message arriving mid-turn (queued for
next turn vs hard interrupt) are not fully characterized here.

## Recursion guard (medium)

For dispatch-capable contexts, a mechanical recursion guard applies to recursive
orchestration/control tools; telemetry-only calls stay allowed, with an explicit
bypass. (This is documented on the bro-harness side; the CC-internal equivalent
is inferred.)

## Open

<!-- TODO(mine): the core loop call graph; max-iteration bound; exact steering/
interrupt merge behavior; how parallel tool results are ordered back into the
messages array; end_turn detection internals. -->

## Feeds

`design/bro-harness/brodex-agent-loop-learnings.md`,
`design/bro-harness/anthropic-harness.md`.

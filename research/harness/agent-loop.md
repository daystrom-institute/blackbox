---
title: "Axis: Agent Loop"
kind: research-axis
corpus: blackbox-research
track: harness
axis: agent-loop
topic:
  - harness
  - agent-loop
brief: "Cross-harness invariant model for the agent-loop axis: the core turn loop a harness runs — turn boundaries, end_turn/stop detection, parallel tool-call handling, tool-result threading, interrupts and mid-flight operator steering, and recursion guards. The control flow that consumes the transport. Synthesis of the per-subject agent-loop cells."
---

# Axis: Agent Loop

> **Scope.** The control flow: the loop that sends a turn, consumes the model's
> tool calls, runs them, threads results back, and decides when the turn is done.
> Consumes the [transport](transport.md); orchestrates the [tool surfaces](builtin-tools.md).
> Not *what* enters the window (see [context-management](context-management.md)).

## The dimension

The agent loop is the harness's heartbeat. Its subtleties decide whether the
agent feels responsive and controllable: how cleanly it detects end-of-turn,
whether it runs tool calls in parallel, how it handles an interrupt or a steering
message that arrives mid-turn, and how it prevents runaway recursion when the
agent can dispatch other agents.

## Questions a finding must answer

- **Turn structure.** What is a "turn"? How is `end_turn` / stop detected? How
  are stop reasons classified (`end_turn`, `max_tokens`, `pause_turn`, tool_use)?
- **Parallel tool calls.** Does the loop execute multiple tool_use blocks
  concurrently? How are results ordered back?
- **Tool-result threading.** How are tool results appended? Padding/repair on
  partial completion?
- **Interrupts & steering.** Can the operator inject a message mid-flight? Is the
  current turn cancelled, queued, or merged? Role-alternation consequences?
- **Recursion / guard.** When the agent can dispatch sub-agents or call
  orchestration tools, what prevents infinite recursion? Explicit bypass?
- **Loop termination.** Max iterations? Idle/empty-turn handling?

## Convergence / divergence

| Subject | Parallel tools | Stop detection | Interrupt model | Recursion guard | Cell |
|---|---|---|---|---|---|
| Claude | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [claude](claude/claude-agent-loop.md) |
| Codex | _TBD_ | env-context + end_turn | _TBD_ | _TBD_ | [codex](codex/codex-agent-loop.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-agent-loop.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-agent-loop.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is parallel-tool-execution universal, or do some harnesses serialize?
- Is there a convergent "steering" model (queue next-turn) vs hard interrupt?

## Codex-lens extensions

- **Per-tool parallel-safety** — whether a tool may run concurrently can be a
  per-tool advisory the model reads (cross-ref [builtin-tools](builtin-tools.md));
  the loop honors it when scheduling parallel calls.
- **Goal-driven continuation** — the loop may inject a "continue toward the goal"
  item each turn from a durable goal contract (cross-ref
  [planning-goals](planning-goals.md)), making turn-to-turn progress
  goal-aware rather than purely reactive.

## Feeds

`design/bro-harness/brodex-agent-loop-learnings.md`,
`design/bro-harness/anthropic-harness.md` (agent loop section). bro-harness
recursion guard: `PROJECT.md` → "Provider & Agent Surfaces".

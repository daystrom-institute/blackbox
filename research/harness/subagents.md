---
title: "Axis: Subagents"
kind: research-axis
corpus: blackbox-research
track: harness
axis: subagents
topic:
  - harness
  - subagents
brief: "Cross-harness invariant model for the subagents axis: how a harness lets an agent spawn other agents — the spawn interface, isolation (worktree/sandbox), result return, parallelism and concurrency caps, agent-type registries, and how a subagent's output is folded back into the parent's context. Synthesis of the per-subject subagent cells."
---

# Axis: Subagents

> **Scope.** Agent-spawns-agent systems: the Task/Agent tool family and its
> machinery. How a subagent is launched, isolated, run, and harvested. Related to
> the [agent-loop](agent-loop.md) recursion guard, but focused on the *delegation
> surface* the parent agent sees.

## The dimension

Subagents are how a harness scales one context across work it can't hold. The
design choices that matter: whether subagents run in isolation (a git worktree,
a sandbox), how their result returns to the parent (final text? structured?),
whether they run in parallel and under what concurrency cap, and whether there is
a registry of specialized agent *types* with distinct tool access and prompts.

## Questions a finding must answer

- **Spawn interface.** What tool launches a subagent? Args (prompt, type, model,
  isolation)? Foreground vs background?
- **Agent-type registry.** Are there named agent types with scoped tools/prompts
  (e.g. read-only explorer, planner)? How discovered?
- **Isolation.** Worktree/sandbox? Shared vs copied working tree? Cleanup?
- **Result return.** What comes back — final message only, or structured output?
  Is it shown to the user or only the parent?
- **Parallelism.** Concurrent spawns? Concurrency cap? Lifetime cap?
- **Continuation.** Can a parent resume a prior subagent with context intact, or
  is every spawn fresh?
- **Context folding.** How does the subagent's output enter the parent window?

## Convergence / divergence

| Subject | Spawn tool | Agent types | Isolation | Parallel? | Cell |
|---|---|---|---|---|---|
| Claude | Task/Agent | yes (registry) | worktree opt-in | yes (capped) | [claude](claude/claude-subagents.md) |
| Codex | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [codex](codex/codex-subagents.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-subagents.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-subagents.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is "final text is the return value" the convergent contract, or do some pass
  structured output?
- Worktree isolation: common, or Claude-specific?

## Codex-lens extensions

- **Lifecycle verbs** — beyond spawn/wait: fork (with a selectable history
  slice), interrupt mid-turn, close (with descendant cascade), resume, list, and
  no-turn `send_message` vs turn-triggering `followup_task`.
- **Role-differentiated tool visibility** — worker sub-agents may see tools the
  orchestrator does not (and vice-versa), gated by session-source identity — a
  harness-enforced role contract, not just a prompt.
- **Fan-out** — CSV/batch dispatch of a worker fleet with per-row templating + a
  structured result schema.
- **Topology** — depth / path (`/root/child`) / persisted parent→child graph is
  owned by [session-lifecycle](session-lifecycle.md).

## Feeds

`design/orchestration/agents/`, `design/orchestration/atoms/`. bro-harness shares
the no-runtime-daemon-dependency invariant — subagent delegation in the harness
must not become an RPC backchannel.

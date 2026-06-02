---
title: "Axis: Hooks"
kind: research-axis
corpus: blackbox-research
track: harness
axis: hooks
topic:
  - harness
  - hooks
brief: "Cross-harness invariant model for the hooks axis: the harness's hook seam — the lifecycle events that fire hooks, the payload each receives, whether a hook is blocking (can deny/modify a tool call) or advisory, and how hook output is injected back into the agent's context. Synthesis of the per-subject hook cells."
---

# Axis: Hooks

> **Scope.** The programmable seam where the operator injects behavior into the
> harness lifecycle. Which events fire, what they receive, what they can do
> (observe / modify / block), and how their output re-enters context. Distinct
> from [skills](skills.md) (agent-invoked capability) — hooks are
> harness-invoked, often without the agent's awareness.

## The dimension

Hooks are the harness's extension mechanism for *deterministic* behavior the
model shouldn't be trusted to perform reliably (formatting, permission gates,
notifications, command rewriting). The key distinctions: which lifecycle points
are hookable, whether a hook can *block or rewrite* a tool call vs merely
observe, and whether the model sees hook output (as feedback) or it's invisible.
(On this host, an RTK hook transparently rewrites shell commands — a live
example of a blocking/rewriting pre-tool hook.)

## Questions a finding must answer

- **Event catalogue.** Which lifecycle events fire hooks? (pre/post tool-use,
  session start/stop, prompt submit, compaction, notification, …)
- **Payload.** What does each event hand the hook (tool name, args, cwd, env)?
- **Blocking vs advisory.** Can a hook deny a tool call? Rewrite its args?
  Inject a different command? Or only observe?
- **Output injection.** Does hook stdout/stderr go back into the model's context
  as feedback? Treated as user input?
- **Configuration.** Where are hooks declared (settings.json?), matching rules
  (tool-name globs?), and precedence?
- **Failure mode.** What happens if a hook errors or times out?

## Convergence / divergence

| Subject | Event set | Blocking? | Output→context | Config surface | Cell |
|---|---|---|---|---|---|
| Claude | _TBD_ | yes (pre-tool deny/rewrite) | yes | settings.json | [claude](claude/claude-hooks.md) |
| Codex | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [codex](codex/codex-hooks.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-hooks.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-hooks.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is pre-tool-use the universal blocking point? Is post-tool-use universally
  advisory-only?
- Do harnesses converge on "hook output is treated as user feedback"?

## Codex-lens extensions

- **Lifecycle breadth** — hooks fire on far more than pre/post-tool: session
  start/stop, prompt-submit, and notably **Pre/PostCompact**, where a hook can
  *abort* compaction (stop authority). A finding should enumerate the full event
  set and, per event, whether the hook can observe / modify / block.

## Feeds

`design/bro-harness/bro-harness-hooks.md` (the harness hook seam + Nudger),
`design/bro-harness/backlog-hooks-catalog-metadata.md`.

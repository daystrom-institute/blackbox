---
title: "Axis: Built-in Tools"
kind: research-axis
corpus: blackbox-research
track: harness
axis: builtin-tools
topic:
  - harness
  - builtin-tools
brief: "Cross-harness invariant model for the built-in-tools axis: the harness's native tool suite — the nature of each tool, the SHAPE of the surface (arg ergonomics, batching, return shapes, error feedback), and crucially the tooldoc steering language that pushes the agent toward a tool and away from anti-patterns. The single highest-value extraction for bro-harness's steer-without-bloat goal. Synthesis of the per-subject built-in-tools cells."
---

# Axis: Built-in Tools

> **Scope.** The harness's *native* tools (file read/edit, shell, search, todo,
> task, etc.) — not MCP tools (see [mcp](mcp.md)). Three lenses: the **nature**
> (what the tool does), the **shape** (how its surface is designed), and the
> **steering language** (the tooldoc wording that guides the agent). The last is
> the prize.

## The dimension

This is where bro-harness's quality bar lives or dies. A built-in tool is not
just a function — it is a *surface* the model reads (the description, the arg
schema, the examples) and a *steer* (the "use this, not that" language). The
mature harnesses spend real tokens on negative guidance ("avoid `cat`/`head`/
`tail`; use Read", "don't `git commit` unless asked") because it measurably
changes agent behavior. Extracting that language — the minimal steer that works —
is the highest-value output of this whole track.

## Questions a finding must answer

- **Inventory.** Which built-in tools ship? Group by family (file, shell,
  search, edit, todo, task, web, …).
- **Nature.** What does each do, and what is deliberately *not* a tool (left to
  shell)?
- **Shape of the surface.** Arg ergonomics (required vs optional, defaults);
  batching (multi-edit, parallel reads); return shapes (line numbers? truncation?
  pagination?); error/feedback shape (how does a failed call teach the agent?).
- **Tooldoc steering language.** The exact "when to use / when NOT to use"
  wording; negative guidance and anti-pattern callouts; context hints embedded in
  descriptions; examples that bias usage. **Capture verbatim, confidence:high,
  minimal — adopt the idiom, not the prose.**
- **Cross-tool steering.** How does the harness route the agent *between* tools
  (e.g. "prefer the dedicated tool over shell")?

## Convergence / divergence

| Subject | File-edit shape | Shell model | Negative-guidance style | Todo/Task | Cell |
|---|---|---|---|---|---|
| Claude | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [claude](claude/claude-builtin-tools.md) |
| Codex | freeform grammar plus dedicated patch | yielded session shell | inline schema/tool-description guidance | plan, goal, agents, context-window controls | [codex](codex/codex-builtin-tools.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-builtin-tools.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-builtin-tools.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is "Read-before-Edit" (mandatory read gate before edit) a universal safety
  idiom?
- Do harnesses converge on line-numbered, truncation-aware read returns?
- What is the minimal negative-guidance set that actually moves behavior?

## Codex-lens extensions

The surface is richer than "description + input schema." A finding should also
cover the **tool I/O contract**:

- **Output schemas** — typed return contracts (JSON Schema for *results*), so the
  model can reason about result shape before it calls.
- **Invocation format** — not every tool is JSON: grammar-constrained / freeform
  tools exist (e.g. an apply-patch tool defined by a Lark grammar). The format
  itself is agent-facing — the model must know not to wrap it in JSON.
- **Per-tool concurrency advisory** — a parallel-safe flag / explicit "do not
  call in parallel" embedded in the tooldoc.
- **Self-describing availability preconditions** — the description (and the
  tool's own error result) teaches the model when it is/isn't available
  (mode/role gated), so it self-selects without a separate prompt section.
- **Agent-authored elicitation** — a tool by which the model asks the operator a
  *structured* question (header / question / options); the inverse of context
  injection (the model produces UI). cf. this harness's `AskUserQuestion`.
- **Context-window controls** - a read-only remaining-token tool and an explicit
  new-window request let the model manage context capacity without conflating it
  with durable goal budgets or resetting environment state.
- **Interruptible wait tools** - sleep/wait surfaces should yield to new user
  input rather than becoming unsteerable blocking calls.

## Feeds

`design/bro-harness/bro-harness-tool-surface.md`,
`design/bro-harness/bro-harness-tool-chaining.md`. This axis is the primary
source for bro-harness's tooldoc language.

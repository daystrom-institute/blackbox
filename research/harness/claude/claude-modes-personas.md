---
title: "Claude · Modes, Personas & Roles"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: modes-personas
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - modes-personas
brief: "Claude decomposes this axis into THREE orthogonal controls: plan mode (EnterPlanMode/ExitPlanMode, read-only+plan, model tied: 'Opus in plan mode, else Sonnet'); output styles (.claude/output-styles/, plugin-extensible — the persona/communication-style layer); and agent types (subagent_type registry, allowedAgentTypes, tool-gated per type). No monolithic persona."
---

# Claude · Modes, Personas & Roles

> Mined from the Claude Code 2.1.160 binary (Bun-compiled JS bundle, `strings` + grep) by a GLM-5.1 bro, 2026-06-02. **confidence: high** (verbatim string literals) + live `~/.claude/` config. This cell was added in the claude *new-axes* pass; two findings **correct** session-1 assumptions (durable goal + memory-consolidation DO exist).
See axis: [Modes, Personas & Roles](../modes-personas.md) · snapshot: [Claude 2.1.160](claude-2.1.160.md).

**Finding.** Three orthogonal systems: (a) **plan mode** — `EnterPlanMode`/`ExitPlanMode`, restricts to read-only + plan file, requires approval to exit; model is tied to mode ("Opus in plan mode, else Sonnet"). (b) **output styles** — `outputStyle` setting loaded from `.claude/output-styles/` (plugin-extensible via `outputStylesPaths`) — the persona/communication-style layer. (c) **agent types** — `subagent_type` on the Agent tool, a registry (`agentDefinitions.activeAgents`, `allowedAgentTypes`, `FORK_SUBAGENT_TYPE`), tool-gated per type; "Available agent types are listed in `<system-reminder>`".

**Evidence.**
- `EnterPlanMode`/`ExitPlanMode` (~266789); "Opus in plan mode, else Sonnet" (~276956)
- `outputStyle` (~275811); `.claude/output-styles/`, `outputStylesPaths`
- `subagent_type`/`agentDefinitions.activeAgents`/`allowedAgentTypes` (~288021)

**Vs the axis.** Confirms all three facets — and shows Claude keeps them **decomposed** (mode ≠ persona ≠ role), the opposite of vibe's *unified* profile/config-overlay. A clean design-fork for the axis synthesis: unify (vibe) vs decompose (claude).

## Open
<!-- output-style content model (system-prompt swap vs style directive); per-agent-type tool gating detail. -->

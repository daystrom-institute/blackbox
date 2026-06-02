---
title: "Vibe · Subagents"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: subagents
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - subagents
brief: "Vibe subagents: a single built-in 'explore' subagent via the Task tool; spawns a new in-memory AgentLoop; SUBAGENT-only (no recursive spawning); runs SEQUENTIALLY (parent blocks); shares parent PermissionStore + approval_callback; custom subagents via TOML in .vibe/agents/."
---

# Vibe · Subagents

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Subagents](../subagents.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** The `task` tool delegates to a subagent: creates a new in-memory `AgentLoop(is_subagent=True)` with its own session-log subdir. Only `AgentType.SUBAGENT` profiles are allowed ("security constraint to prevent recursive spawning"). Built-in `explore` profile = read-only (`grep`, `read_file`) + `explore` system prompt. Subagents run **sequentially** (parent `async with aclosing(subagent.act(...))`), not fanned out. Result = accumulated text + turn count + completion flag. Subagent shares the parent's `PermissionStore` and `approval_callback`; scratchpad path is injected. Custom subagents via TOML in `.vibe/agents/` or `~/.vibe/agents/`.

**Evidence.**
- `vibe/core/tools/builtins/task.py:131` — "Only subagents can be used … prevent recursive spawning"
- `vibe/core/agents/models.py:88` — `EXPLORE` profile (read-only toolset)
- `vibe/core/tools/builtins/task.py:171` — sequential `aclosing(subagent.act(...))`

**Vs the axis.** Confirms a typed subagent registry + recursion guard. **Divergence:** vibe is **sequential, single-type** (no parallel fan-out, no fork/interrupt lifecycle verbs like codex).

## Open
<!-- Whether custom .vibe/agents TOML can define non-explore subagent toolsets in practice. -->

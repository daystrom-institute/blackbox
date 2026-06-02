---
title: "Vibe · Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: agent-loop
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - agent-loop
brief: "Vibe loop: _conversation_loop with a middleware pipeline; CONCURRENT tool execution (asyncio.create_task + queue); breaks when last msg role != tool; budget/limit middleware (turns/price/tokens); mid-session agent switch; hooks can inject retry messages post-turn."
---

# Vibe · Agent Loop

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Agent Loop](../agent-loop.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** `_conversation_loop`: append user msg → `while not should_break` → middleware runs → `_perform_llm_turn` (streaming/non) → if tool_calls, run **concurrently** via `asyncio.create_task` + `asyncio.Queue` → append results → loop. Breaks when the last message role ≠ `tool`. Programmatic knobs enforced as middleware: `max_turns`, `max_price`, `max_session_tokens`. `switch_agent` swaps the active profile mid-session. Hooks (`HooksManager`) can inject a retry message after the agent turn. Public entry `act()` is an async generator of events; programmatic mode wraps it with `asyncio.run`.

**Evidence.**
- `vibe/core/agent_loop.py:869` — `_conversation_loop`
- `vibe/core/agent_loop.py:1231` — `_run_tools_concurrently` (task-per-call, queue fan-out)
- `vibe/core/middleware.py:82` — Turn/Price/Token/AutoCompact middleware

**Vs the axis.** Confirms concurrent tool exec + clean turn-boundary. **Extends:** the **middleware pipeline** is the control-flow seam (limits, compaction, mode reminders all ride it) — a clean generalization of "operator steering."

## Open
<!-- Interrupt semantics mid-tool; ordering guarantees on concurrent tool results. -->

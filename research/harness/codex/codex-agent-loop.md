---
title: "Codex · Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: agent-loop
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - agent-loop
brief: "Codex loop: stream → process SSE (OutputItemAdded/Done, text/reasoning deltas, Completed) → if Completed.end_turn==Some(false) loop (follow-up) else stop. Parallel tool dispatch via FuturesOrdered (in_flight, drained at Completed), gated by parallel_tool_calls. Mailbox preemption breaks early to process pending mail; a TurnDiff event emits a unified diff after each turn."
---

# Codex · Agent Loop

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Agent Loop](../agent-loop.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** `run_sampling_request` loops: build prompt → `stream()` → process SSE → if `Completed.end_turn == Some(false)` run a **follow-up** turn, else (`None`/`Some(true)`) return. SSE handling: `OutputItemAdded` registers active items (+ a `ToolArgumentDiffConsumer` for function calls); `OutputItemDone` finalizes tool calls (dispatch + schedule) and assistant text; reasoning deltas forwarded as events; `Completed` records usage + `end_turn`. **Parallel tool dispatch:** tool futures collected in a `FuturesOrdered` (`in_flight`), awaited at `Completed` (`drain_in_flight`), gated by `supports_parallel_tool_calls`. **Mailbox preemption:** a completed Reasoning/Commentary item with pending mailbox mail breaks the loop early. Post-turn `EventMsg::TurnDiff` emits a unified diff of file changes.

**Evidence.**
- `core/src/session/turn.rs` (~916-970) — `run_sampling_request` loop + `end_turn` follow-up
- `core/src/session/turn.rs` (1070-2200) — SSE event handling
- `core/src/tools/parallel.rs:30` — `ToolCallRuntime`; `FuturesOrdered` drain

**Vs the axis.** Confirms parallel tool calls + end_turn detection + interrupt handling. Extends with **mailbox preemption** (inter-agent mail mid-turn) and the per-turn TurnDiff surface.

## Open
<!-- Mailbox semantics (multi-agent mail); ToolArgumentDiffConsumer streaming detail. -->

---
title: "Antigravity · Subagents"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: subagents
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - subagents
brief: "SDK confirms subagents as a builtin start_subagent capability enabled by default in CapabilitiesConfig. LocalConnection tracks active_subagent_ids and subagent responses; parent idle waits until active subagents complete. CLI strings/keybindings add specialized subagent names, fast/heavy tiers, and approval/navigation shortcuts."
---

# Antigravity · Subagents

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Subagents](../subagents.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** `InvokeSubagentToolConfig`: `GetFastModel`/`GetHeavyModel` (per-subagent model tier), `GetMaxNestingDepth` (recursion cap), `GetAllowTaskMode`. Named subagent types: `antigravity_browser` (web), `knowledge_retrieval`, `knowledge_past_work`, `implementation_plan`. Default 60s interaction timeout is scoped to subagents (v1.0.2). UI: `KeySubagentApprove`/`KeySubagentApproveFast`/`KeySubagentRespond`; statusline reads active subagent count.

**Evidence.**
- `InvokeSubagentToolConfig{GetFastModel,GetHeavyModel,GetMaxNestingDepth,GetAllowTaskMode}`
- subagent types: `antigravity_browser`, `knowledge_retrieval`, `knowledge_past_work`
- CHANGELOG v1.0.2: 60s timeout scoped to subagents

**Vs the axis.** Confirms a typed subagent registry + **per-subagent model-tier selection** (fast/heavy) + depth cap — extends the axis with model-tier routing as a first-class subagent control.

## SDK/local harness update (2026-06-02)

The SDK exposes subagents as a first-class builtin, start_subagent, backed by the localharness invoke_subagent field. CapabilitiesConfig has enable_subagents and the reference examples describe subagents as enabled by default. A subagent call is not just a display event; LocalConnection tracks active_subagent_ids and subagent response queues.

Idle semantics are subagent-aware. The parent conversation is not considered idle while SDK-tracked subagents are active, and subagent outputs are aggregated back to the main agent. That matters for harness design because subagent orchestration is part of the main loop's completion condition.

The closed CLI adds richer specialization signals: fast/heavy model tiers, named subagent types such as browser/knowledge/plan roles, max nesting depth, task-mode controls, a subagent-scoped interaction timeout, and local keybindings for approve/jump actions. Those remain medium confidence where source is binary strings or local keybinding names.

## Open
<!-- fast-vs-heavy routing heuristic; whether subagents run in parallel. -->

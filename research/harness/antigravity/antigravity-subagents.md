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
confidence: medium
topic:
  - harness
  - antigravity
  - subagents
brief: "agy subagents: InvokeSubagentToolConfig with fast/heavy model tiers, MaxNestingDepth, AllowTaskMode; named types (antigravity_browser, knowledge_retrieval, knowledge_past_work, implementation_plan); 60s interaction timeout scoped to subagents; keyboard approve/respond shortcuts; statusline shows active subagent count."
---

# Antigravity · Subagents

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Subagents](../subagents.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** `InvokeSubagentToolConfig`: `GetFastModel`/`GetHeavyModel` (per-subagent model tier), `GetMaxNestingDepth` (recursion cap), `GetAllowTaskMode`. Named subagent types: `antigravity_browser` (web), `knowledge_retrieval`, `knowledge_past_work`, `implementation_plan`. Default 60s interaction timeout is scoped to subagents (v1.0.2). UI: `KeySubagentApprove`/`KeySubagentApproveFast`/`KeySubagentRespond`; statusline reads active subagent count.

**Evidence.**
- `InvokeSubagentToolConfig{GetFastModel,GetHeavyModel,GetMaxNestingDepth,GetAllowTaskMode}`
- subagent types: `antigravity_browser`, `knowledge_retrieval`, `knowledge_past_work`
- CHANGELOG v1.0.2: 60s timeout scoped to subagents

**Vs the axis.** Confirms a typed subagent registry + **per-subagent model-tier selection** (fast/heavy) + depth cap — extends the axis with model-tier routing as a first-class subagent control.

## Open
<!-- fast-vs-heavy routing heuristic; whether subagents run in parallel. -->

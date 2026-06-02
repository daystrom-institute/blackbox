---
title: "Antigravity · Agent Loop"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: agent-loop
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - agent-loop
brief: "agy loop is server-side; client streams/renders step updates. Protobuf: ToolTurnLimit, YieldInfo (RemainingSteps, CompletedStepResponses, per-step tool/prompt), subagent fast/heavy model tiers, disable_loop_detection opt-out. 60s interaction timeout scoped to subagents only."
---

# Antigravity · Agent Loop

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Agent Loop](../agent-loop.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** The loop runs server-side; the client streams and renders step updates. Protobuf: `ToolConfig.GetToolTurnLimit`; `YieldInfo{GetRemainingSteps, GetCompletedStepResponses, Step{GetTool,GetPrompt}}` (yield/step accounting); `InvokeSubagentToolConfig` with fast/heavy model tiers; `disable_loop_detection` opt-out (loop detection in `jetski/cortex/utils/loop_detection.go`, "[ignoring loop detection]"). CHANGELOG v1.0.2: the default 60s interaction timeout was restricted to subagents only.

**Evidence.**
- `YieldInfo{GetRemainingSteps,GetCompletedStepResponses}`
- `disable_loop_detection` (agent config varint), "[ignoring loop detection]"
- CHANGELOG v1.0.2: 60s timeout scoped to subagents

**Vs the axis.** Confirms turn/step limits + a **server-side loop-detection** guard (a robustness/loop axis crossover). **Divergence:** no autonomous client loop — the client is a step-renderer.

## Open
<!-- Server step-loop semantics; end-of-turn signaling over gRPC. -->

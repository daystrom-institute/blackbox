---
title: "Antigravity · Compaction"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: compaction
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - compaction
brief: "agy compaction is SERVER-SIDE: CompactionInfo records compacted step indices; client reconstructCompactedIndices reconstitutes; ContextWindowMetadata carries EstimatedTokensUsed + TruncationReason; MemoryConfig.CondenseInputTrajectory. Live context-window % is surfaced to the statusline."
---

# Antigravity · Compaction

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Compaction](../compaction.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Server-side compaction with **step-index tracking**: `CompactionInfo.GetCompactedAtStepIndices` records which steps were compacted; the client's `AgentState.reconstructCompactedIndices` reconstitutes them. `ContextWindowMetadata` exposes `EstimatedTokensUsed` + `TruncationReason`; `MemoryConfig.GetCondenseInputTrajectory` condenses the trajectory. No client-side trigger/summarizer — it's a cortex operation. Live `context_window.used_percentage` is piped to the statusline hook.

**Evidence.**
- `jetski_cortex_pb.CompactionInfo{GetCompactedAtStepIndices}`; `AgentState.reconstructCompactedIndices`
- `ContextWindowMetadata{GetEstimatedTokensUsed,GetTruncationReason}`

**Vs the axis.** Confirms server-side compaction (extends the codex server-side compaction lane) with an explicit **step-index reconstruction** contract the client must honor — a post-compact-history-shape mechanism. Trigger/prompt are not client-visible.

## Open
<!-- Server-side trigger math + summarizer prompt (not in the binary). -->

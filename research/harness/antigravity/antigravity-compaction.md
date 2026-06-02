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
brief: "SDK confirms compaction as first-class step metadata: CapabilitiesConfig has compaction_threshold, Conversation records StepType.COMPACTION indices, and OnCompaction hooks observe compaction events. CLI binary strings still suggest server-side compaction info and context-window statusline reporting."
---

# Antigravity · Compaction

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Compaction](../compaction.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Server-side compaction with **step-index tracking**: `CompactionInfo.GetCompactedAtStepIndices` records which steps were compacted; the client's `AgentState.reconstructCompactedIndices` reconstitutes them. `ContextWindowMetadata` exposes `EstimatedTokensUsed` + `TruncationReason`; `MemoryConfig.GetCondenseInputTrajectory` condenses the trajectory. No client-side trigger/summarizer — it's a cortex operation. Live `context_window.used_percentage` is piped to the statusline hook.

**Evidence.**
- `jetski_cortex_pb.CompactionInfo{GetCompactedAtStepIndices}`; `AgentState.reconstructCompactedIndices`
- `ContextWindowMetadata{GetEstimatedTokensUsed,GetTruncationReason}`

**Vs the axis.** Confirms server-side compaction (extends the codex server-side compaction lane) with an explicit **step-index reconstruction** contract the client must honor — a post-compact-history-shape mechanism. Trigger/prompt are not client-visible.

## SDK/local harness update (2026-06-02)

The SDK exposes compaction at three levels. CapabilitiesConfig includes compaction_threshold, so callers can tune when the harness should compact. Conversation.receive_steps records indices for steps whose type is COMPACTION, making compaction part of the durable step history rather than an invisible truncation side effect. HookRunner also dispatches OnCompaction hooks, so user code can observe compaction events.

The SDK does not reveal the summarizer prompt or compaction algorithm. The standalone CLI binary strings still point at server-side compaction metadata, reconstructed compacted indices, context-window token estimates, and statusline display. Treat algorithmic claims as medium confidence until a live compaction transcript or source-level server implementation is available.

## Open
<!-- Server-side trigger math + summarizer prompt (not in the binary). -->

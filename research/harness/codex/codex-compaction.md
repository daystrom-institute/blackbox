---
title: "Codex · Compaction"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: compaction
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - compaction
brief: "Codex compaction has THREE provider-dispatched variants: remote-v2 (streaming, returns a ResponseItem::Compaction), remote-v1 (/responses/compact endpoint), inline (local summarizer) — gated by supports_remote_compaction (openai||azure) then the RemoteCompactionV2 flag. InitialContextInjection (DoNotInject pre-turn vs BeforeLastUserMessage mid-turn) sets the post-compact history shape; summary becomes the last assistant msg with SUMMARY_PREFIX. Pre-sampling trigger; PreCompact hook can abort."
---

# Codex · Compaction

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Compaction](../compaction.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Three variants selected by `should_use_remote_compact_task` (`supports_remote_compaction` = openai||azure) then `Feature::RemoteCompactionV2`: **remote-v2** (streaming via the normal `stream()` path + a `CompactionTrigger`, collects one `ResponseItem::Compaction`, `RETAINED_MESSAGE_TOKEN_BUDGET=64_000`), **remote-v1** (`/responses/compact`, trims function-call history to fit first), **inline** (local summarizer). `InitialContextInjection` controls post-compact shape: `DoNotInject` (pre-turn/manual — next turn reinjects normally) vs `BeforeLastUserMessage` (mid-turn — context slotted just above the last real user msg, since the model expects the summary last). Summary becomes the last assistant message (`SUMMARY_PREFIX`), preserved user messages kept, rest dropped. Pre-sampling trigger via `auto_compact_token_status`; `PreCompactHookOutcome` can Stop/Proceed.

**Evidence.**
- `core/src/compact.rs:47-67` — variant selection; `InitialContextInjection` enum
- `core/src/compact_remote_v2.rs:50-55` — `MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES=2`, `RETAINED_MESSAGE_TOKEN_BUDGET=64000`
- `core/src/compact_remote.rs:85-130` — `/compact` + `trim_function_call_history_to_fit_context_window`

**Vs the axis.** Strongly confirms the implementation-variant-dispatch + post-compact-history-shape extensions, and anchors the server-side compaction lane (vs Claude/vibe client-side, agy server-side step-index).

## Open
<!-- Server-side summarizer prompt (remote path); pre-sampling threshold constants. -->

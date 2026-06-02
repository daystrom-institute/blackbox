---
title: "Claude · Session Lifecycle & History"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: session-lifecycle
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - session-lifecycle
brief: "Claude sessions: --continue / --resume[=id]; .jsonl transcripts under ~/.claude/projects/; /compact (PreCompact/PostCompact hooks, can BLOCK compaction); /rewind restores via per-message FILE snapshots (fileCheckpointingEnabled, default on); autoCompactEnabled default on; transcriptRetentionDays=30."
---

# Claude · Session Lifecycle & History

> Mined from the Claude Code 2.1.160 binary (Bun-compiled JS bundle, `strings` + grep) by a GLM-5.1 bro, 2026-06-02. **confidence: high** (verbatim string literals) + live `~/.claude/` config. This cell was added in the claude *new-axes* pass; two findings **correct** session-1 assumptions (durable goal + memory-consolidation DO exist).
See axis: [Session Lifecycle & History](../session-lifecycle.md) · snapshot: [Claude 2.1.160](claude-2.1.160.md).

**Finding.** `--continue` (last session) / `--resume[=<id>]`. Transcripts = `.jsonl` (one JSON/line) under `~/.claude/projects/`; `loadTranscriptFromFile` reconstructs (and `restoreGoalFromTranscript` rehydrates the goal). **`/compact`** triggers summarization, gated by `PreCompact`/`PostCompact` hooks (a `PreCompact` hook can **block** it: "Compaction blocked by PreCompact hook"); `autoCompactEnabled` default on; "[earlier conversation truncated for compaction retry]". **`/rewind`** restores to a prior point using per-message **file snapshots** ("Snapshot files before edits so /rewind can restore them"; `fileCheckpointingEnabled` default true; `CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING`). `transcriptRetentionDays` default 30.

**Evidence.**
- `--continue`/`--resume` (~274547); ".jsonl files under the projects directory" (~268210)
- `PreCompact`/`PostCompact` (~268526); "Compaction blocked by PreCompact hook" (~267919)
- "Snapshot files before edits so /rewind can restore them" (~275726); `fileCheckpointingEnabled`

**Vs the axis.** Confirms resume + rewind (file-snapshot based) + hook-gated compaction. **4-way convergence on rewind** (claude /rewind, codex rollback, vibe RewindManager, agy "rewind to step") — file-snapshot restore is shared by claude+vibe. Crosscuts compaction (PreCompact hook) and hooks axes.

## Open
<!-- rewind granularity (message vs checkpoint); interaction of rewind with the durable goal. -->

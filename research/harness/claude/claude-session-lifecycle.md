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
brief: "Claude sessions: --continue / --resume[=id], --fork-session, --session-id, --no-session-persistence, --from-pr, .jsonl transcripts under ~/.claude/projects/, file-history snapshots for /rewind, hook-gated compaction, literal /recap session-recap generation, and a background agent session manager via claude agents with JSON listing and dispatch defaults."
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

## CLI Lifecycle Deltas (2026-06-02 local pass)

Current help adds several lifecycle flags beyond the original resume/compact/rewind summary:

- --fork-session creates a new session ID when resuming instead of reusing the original.
- --session-id lets callers provide a specific UUID.
- --no-session-persistence disables disk persistence for print mode, so the session cannot be resumed.
- --from-pr resumes a session linked to a PR by number/URL or opens a picker.
- --name sets a display name used in the prompt box, /resume picker, and terminal title.
- --include-hook-events and --include-partial-messages make lifecycle and partial output observable in stream-json mode.

Background sessions are now a separate lifecycle surface. claude agents can list live sessions as JSON, filter by cwd, and apply dispatch defaults for settings, MCP config, strict MCP config, plugin dirs, permission mode, model, effort, agent, add-dir, and bypass availability. Changelog entries also show resume support for background sessions, worktree cleanup/retention behavior, pinned sessions, and preservation of permission/model/effort state across detach/retire/wake.

## Recap / Away Summary

Claude has a literal local slash command named `recap`, described in the 2.1.160 binary as "Generate a one-line session recap now". The command calls the same generator as the automatic return-to-session recap path and returns local text outcomes for no prior turn ("Nothing to recap yet - send a message first."), cancellation, API error text, or generation failure.

Internally this surface is named `awaySummary`. The generator uses saved `CacheSafeParams` from the current session; if none exist it reports `kind: "no-turn"`. It then runs one model turn with `querySource: "away_summary"`, `forkLabel: "away_summary"`, `maxTurns: 1`, `skipTranscript: true`, and `skipCacheWrite: true`. Tools are denied with the explicit decision reason `away_summary` and message "Away summary cannot use tools". The hardcoded recap instruction asks for under 40 words, 1-2 plain sentences, no markdown, leading with overall goal/current task and the one next action.

Automatic recap is gated separately from the manual `/recap` command. The config setting is `awaySummaryEnabled`, labeled "Session recap" in `/config`; the env var `CLAUDE_CODE_ENABLE_AWAY_SUMMARY` can force enable/disable. The binary default generation delay is 180000 ms, while changelog/config prose describes the user-facing feature as shown when returning after being away for 5+ minutes. The automatic path skips generation when cache age is unknown or stale, when near rate limit, when draft input is present, when there are fewer than three real user messages, when fewer than two real user messages have arrived since a prior `away_summary`, or when the latest non-metrics message is already an `away_summary`.

When automatic generation succeeds, Claude appends a system message with subtype `away_summary`; for the first three displayed recaps it suffixes "(disable recaps in /config)". Manual `/recap` displays the generated text as local slash-command output. Changelog entries corroborate the feature lineage: 2.1.108 added recap, configurable in `/config` and manually invocable with `/recap`; 2.1.110 enabled it for telemetry-disabled users and fixed focus-mode display; 2.1.112 fixed auto-firing while composing unsent text.

## Open
<!-- rewind granularity (message vs checkpoint); interaction of rewind with the durable goal. -->

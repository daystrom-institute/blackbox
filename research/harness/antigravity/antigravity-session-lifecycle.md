---
title: "Antigravity · Session Lifecycle & History"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: session-lifecycle
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - session-lifecycle
brief: "SDK Conversation is stateful and resumable through conversation_id plus save_dir; it tracks history, turn_count, last_response, usage, compaction indices, idle/wakeup, cancel, delete, clear_history, and disconnect. Local agy 1.0.4 stores trajectory steps in SQLite and brain JSONL transcripts under ~/.gemini/antigravity-cli."
---

# Antigravity · Session Lifecycle & History

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Session Lifecycle & History](../session-lifecycle.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Conversation storage is transitioning from JSON (`~/.gemini/tmp/<project>/chats/session-*.json`, v1.0.1) to **SQLite (.db)** as the canonical CLI format (v1.0.4; "trajectory db schema init", import from Antigravity 2.0). Full lifecycle: new ("Starting new conversation"), continue ("Continuing last-used conversation (from cache)"), **resume** (`/resume`, "Print mode: resuming conversation"), list/browse, import ("Import this conversation?"), **export to the 2.0 IDE**, cancel (`conversation_cancelled`), delete, and **rewind** ("Rewinding conversation %s to step %d"). Session JSON: `sessionId`, `projectHash`, `startTime`, `lastUpdated`; messages typed `user`/`gemini`/`error`/`info` with `thoughts`/`toolCalls`/`tokens`.

**Evidence.**
- CHANGELOG v1.0.4: "Added SQLite (.db) conversation support and will be CLI's conversation format"
- strings: "Rewinding conversation %s to step %d"; "Continuing last-used conversation (from cache)"
- session JSON fields: `sessionId`, `projectHash`, message types user/gemini/error/info

**Vs the axis.** Confirms resume/rewind + CLI↔IDE session export (a cross-surface handoff none other has). **4-way convergence on rewind** (codex rollback / claude /rewind / vibe RewindManager / agy "rewind to step"). Storage uniquely moving to SQLite.

## SDK/local harness update (2026-06-02)

The SDK Conversation object is the session handle. It keeps local step history, turn start indices, compaction indices, usage totals, and the last response. It exposes chat/send streaming, history, turn_count, last_response, total_usage, last_turn_usage, is_idle, wait_for_idle, wait_for_wakeup, signal_idle, clear_history, cancel, delete, disconnect, and conversation_id.

Persistence is explicit. AgentConfig accepts conversation_id for resume, save_dir for persisted conversation state, and app_data_dir for artifacts/scratch/media. The persistence example requires the same save_dir when resuming by conversation_id. clear_history only clears local Conversation history; delete removes the backend/local-harness conversation.

Local standalone agy state lines up with a trajectory-store model. The observed SQLite DB has trajectory_meta, steps, gen_metadata, executor_metadata, parent_references, trajectory_metadata_blob, and battle_mode_infos. Brain logs contain typed transcript JSONL. This is stronger than the earlier JSON-to-SQLite migration claim, but the exact step_type enum mapping in the CLI DB still needs decoding.

## Open
<!-- SQLite schema (no .db on host yet); step-granular rewind semantics vs message-granular. -->

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
brief: "agy sessions: transitioning JSON (~/.gemini/tmp/<project>/chats/session-*.json) → SQLite (.db) as of v1.0.4 (the new canonical format; import from Antigravity 2.0). Full CRUD: new/continue/resume(/resume)/list/import/export-to-IDE/cancel/delete, plus REWIND ('Rewinding conversation %s to step %d'). Metadata: sessionId/projectHash/startTime; messages typed user/gemini/error/info with thoughts/toolCalls/tokens."
---

# Antigravity · Session Lifecycle & History

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Session Lifecycle & History](../session-lifecycle.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Conversation storage is transitioning from JSON (`~/.gemini/tmp/<project>/chats/session-*.json`, v1.0.1) to **SQLite (.db)** as the canonical CLI format (v1.0.4; "trajectory db schema init", import from Antigravity 2.0). Full lifecycle: new ("Starting new conversation"), continue ("Continuing last-used conversation (from cache)"), **resume** (`/resume`, "Print mode: resuming conversation"), list/browse, import ("Import this conversation?"), **export to the 2.0 IDE**, cancel (`conversation_cancelled`), delete, and **rewind** ("Rewinding conversation %s to step %d"). Session JSON: `sessionId`, `projectHash`, `startTime`, `lastUpdated`; messages typed `user`/`gemini`/`error`/`info` with `thoughts`/`toolCalls`/`tokens`.

**Evidence.**
- CHANGELOG v1.0.4: "Added SQLite (.db) conversation support and will be CLI's conversation format"
- strings: "Rewinding conversation %s to step %d"; "Continuing last-used conversation (from cache)"
- session JSON fields: `sessionId`, `projectHash`, message types user/gemini/error/info

**Vs the axis.** Confirms resume/rewind + CLI↔IDE session export (a cross-surface handoff none other has). **4-way convergence on rewind** (codex rollback / claude /rewind / vibe RewindManager / agy "rewind to step"). Storage uniquely moving to SQLite.

## Open
<!-- SQLite schema (no .db on host yet); step-granular rewind semantics vs message-granular. -->

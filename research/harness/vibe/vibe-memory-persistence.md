---
title: "Vibe · Memory & Persistence"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: memory-persistence
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - memory-persistence
brief: "Vibe has NO cross-session durable model-writable memory. Persistence is limited to static (human-authored) AGENTS.md, read-only session JSONL logs, persisted permission rules, and session-chained compaction summaries. No save_memory/load_memory tool, no extract→consolidate pipeline."
---

# Vibe · Memory & Persistence

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high** (incl. a confirming absence). See axis: [Memory & Persistence](../memory-persistence.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** **No model-writable cross-session memory.** Cross-session state is only: (1) `AGENTS.md` (static, human-authored, walked project-up + `~/.vibe/AGENTS.md`); (2) session JSONL logs (`~/.vibe/sessions/`, read-only history for resume); (3) persisted permission "always" rules in config; (4) compaction summaries (session-chained, not a memory store). A content search for `save_memory`/`load_memory`/`MEMORY.md` returns nothing.

**Evidence.**
- `content_search save_memory|load_memory|MEMORY\.md` — no matches
- `vibe/core/system_prompt.py:345` — AGENTS.md loading (static overlays)
- `vibe/core/session/session_loader.py` — `meta.json` + `messages.jsonl` (read-only)

**Vs the axis.** **Not present.** Sharpens the axis: vibe is the negative case — durable memory is the dividing line between harnesses (codex pipeline + Claude auto-dream **present**; vibe **absent**).

## Open
<!-- Whether Nuage/Vibe-Code remote sessions add any server-side memory not visible in the OSS client. -->

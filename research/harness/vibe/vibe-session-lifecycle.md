---
title: "Vibe · Session Lifecycle & History"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: session-lifecycle
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - session-lifecycle
brief: "Vibe sessions: ~/.vibe/sessions/ (meta.json + messages.jsonl), --continue (latest for cwd) / --resume [id] (picker or specific), parent_session_id chain; fork(message_id) creates a child loop; RewindManager checkpoints (file snapshots + msg index) enable rewind_to_message with file restore; compaction creates a chained child session."
---

# Vibe · Session Lifecycle & History

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Session Lifecycle & History](../session-lifecycle.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Sessions persist under `~/.vibe/sessions/` as `meta.json` (id, title, env, `parent_session_id`, timestamps) + `messages.jsonl` (one msg/line). `--continue` loads the latest for cwd; `--resume [id]` shows a picker or loads a specific one (local + remote Nuage sessions). **Fork:** `fork(message_id)` builds a new loop with messages up to/from a message, recording the parent. **Rewind:** `RewindManager` snapshots files + message indices before each user message; `rewind_to_message(restore_files=True)` truncates messages, restores file snapshots, then forks to a new session. Compaction also creates a chained child session. No tag/branch beyond `parent_session_id`.

**Evidence.**
- `vibe/core/session/session_loader.py` — `meta.json`+`messages.jsonl`, find_latest/by_id
- `vibe/core/agent_loop.py:1589` — `fork`, `_messages_for_fork`
- `vibe/core/rewind/manager.py` — checkpoints + `rewind_to_message`

**Vs the axis.** Confirms resume/fork/rollback + topology (parent chain). **Idiom:** rewind = message-truncation + **file-snapshot restore** + fork (mirrors codex's rollback and Claude's `/rewind` file checkpointing — a 3-way convergence).

## Open
<!-- Remote (Nuage) session model; whether forks form a navigable tree in the UI. -->

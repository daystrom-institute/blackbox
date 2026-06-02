---
title: "Claude · Memory & Persistence"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: memory-persistence
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - memory-persistence
brief: "Claude HAS a cross-session memory pipeline: 'auto-dream' background consolidation (autoMemoryEnabled), personal + team memory sync (TEAM_MEMORY_SYNC_URL, personal-memory-sync) via a server API (/api/claude_code/memory), storage at ~/.claude/projects/<cwd>/memory/, the /memory command writes CLAUDE.md. Corrects the session-1 'static overlays only' assumption."
---

# Claude · Memory & Persistence

> Mined from the Claude Code 2.1.160 binary (Bun-compiled JS bundle, `strings` + grep) by a GLM-5.1 bro, 2026-06-02. **confidence: high** (verbatim string literals) + live `~/.claude/` config. This cell was added in the claude *new-axes* pass; two findings **correct** session-1 assumptions (durable goal + memory-consolidation DO exist).
See axis: [Memory & Persistence](../memory-persistence.md) · snapshot: [Claude 2.1.160](claude-2.1.160.md).

**Finding.** **Correction to session-1:** Claude is NOT just static CLAUDE.md. There IS a cross-session extract→consolidate pipeline: **"auto-dream"** background memory consolidation (`autoMemoryEnabled` setting, `memoryUsageCount`), **personal memory sync** (`personal-memory-sync`, `isPersonalMemorySyncEnabled`, `getPersonalMemPath`) and **team memory sync** (`TEAM_MEMORY_SYNC_URL`, "organization-managed memory"), backed by a **server API** `/api/claude_code/memory?scope=user&repo=`. Auto-memory storage defaults to `~/.claude/projects/<sanitized-cwd>/memory/`. CLAUDE.md is still the hierarchical overlay surface (`getClaudeMds`); the `/memory` command writes it; `CLAUDE_CODE_DISABLE_CLAUDE_MDS` disables loading. On this host `autoMemoryEnabled:false` (operator-disabled).

**Evidence.**
- `"Enable background memory consolidation (auto-dream)"` (~275701); `autoMemoryEnabled` (settings.json: false)
- `TEAM_MEMORY_SYNC_URL` (~270297); `/api/claude_code/memory?scope=user&repo=` (~304958)
- `"…auto-memory storage… defaults to ~/.claude/projects/<sanitized-cwd>/memory/"` (~275700)

**Vs the axis.** Confirms cross-session durable memory + extract→consolidate (auto-dream) + server sync — a *third* memory architecture (codex: text extract→consolidate sub-agent; agy: versioned brain artifacts + retrieval subagents; **claude: auto-dream consolidation + personal/team sync**). vibe remains the negative case.

## Open
<!-- auto-dream trigger/cadence; what the consolidation prompt extracts; CRUD via the server API. -->

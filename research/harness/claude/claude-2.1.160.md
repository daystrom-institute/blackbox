---
title: "Claude Code — 2.1.160 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: claude
version: "2.1.160"
platform: macos-aarch64
captured: "2026-06-02"
supersedes: null
status: enriched
topic:
  - harness
  - claude
brief: "Point-in-time research snapshot for Claude Code 2.1.160: provenance plus the per-axis checklist. Deepened 2026-06-02 with current macOS arm64 CLI help, local settings/changelog, auto-mode introspection, and binary-string hook catalogue evidence."
---

# Claude Code — 2.1.160 (snapshot)

This is the **temporal anchor** for the `claude` subject at version 2.1.160. It
holds provenance once (leaves do not repeat it) and a checklist that coordinates
research across the [ten axes](../harness-tracks.md#3-the-axes-what-we-study).

> **This snapshot is enriched, with some internals still open.** Each leaf
> carries evidence from existing bro-harness binary mines, direct observation of
> the running 2.1.160 harness, current CLI help, local settings/changelog, and a
> focused binary-string pass over hooks/permissions/background-agent surfaces.

## Provenance

- **Subject:** Claude Code (Anthropic's official CLI).
- **Version:** 2.1.160 (current installed; `claude --version` → `2.1.160 (Claude Code)`).
- **Binary:** `/Users/invidious/.local/share/claude/versions/2.1.160` — Mach-O
  64-bit arm64 executable, ~207.4 MB (Bun-compiled single file; the JS bundle and
  many string literals are embedded as plaintext).
- **Host:** macOS arm64.
- **Extraction methods available:**
  - `strings -n1 <binary> | grep …` over the embedded bundle — recovers verbatim
    string literals (prompts, tool descriptions, reminder text) at **high**
    confidence, and minified logic (threshold math, call graphs) at **medium**.
  - **Direct observation** of the running harness — tool schemas, `<system-reminder>`
    text, the skills list, deferred-tool disclosure, the env/first-turn block.
    High confidence for what is in-band; does not expose internal call graphs.
- **Prior mines (cross-ref).** `design/bro-harness/compaction-canonical-anthropic.md`
  and `design/bro-harness/bro-harness-api-robustness.md` mined a 2.1.160 install
  (arm64 macOS) for the compaction and Anthropic-transport idioms. Those findings
  are cited by the relevant leaves below.
- **Legal posture.** Interop understanding. Verbatim evidence may live in these
  leaves; do not paste proprietary prompt prose into shipped harness code (adopt
  the idiom). See the [charter §8](../harness-tracks.md#8-the-shape--nature-of-mining).

## Update (2026-06-02 · mining pass)

All 15 axis cells for Claude are now populated. The 10 base-axis cells were
backfilled from direct observation (session 1); the **5 governance-axis cells**
(privilege-approvals, planning-goals, memory-persistence, modes-personas,
session-lifecycle) were added this pass from a GLM-5.1 **binary mine** of the
2.1.160 bundle (`confidence: high`, verbatim). **Two corrections to session-1
assumptions:** Claude *does* have a durable cross-session goal (`activeGoal`,
restored on resume) and a memory-consolidation pipeline (`auto-dream` + personal/
team sync). Per-cell frontmatter is authoritative for `status`/`confidence`; the
table below is not re-statused.

## Update (2026-06-02 · local CLI deepening pass)

Current `claude --help`, `~/.claude/settings.json`, `~/.claude/cache/changelog.md`,
`claude auto-mode defaults`, and focused binary strings add these deltas:

- Hooks are no longer merely `researching`: the event catalogue includes
  PermissionRequest, PreToolUse, PostToolUse, PostToolUseFailure, Notification,
  UserPromptSubmit, SessionStart, SessionEnd, SubagentStart, SubagentStop,
  PreCompact, PostCompact, Setup, ConfigChange, and MessageDisplay. Hook JSON can
  return systemMessage, additionalContext, permissionDecision, updatedInput,
  terminalSequence, reloadSkills, and sessionTitle depending on event.
- Permission mode choices exposed by help are default, plan, auto, acceptEdits,
  bypassPermissions, and dontAsk. Auto mode has an inspectable classifier with
  allow / soft_deny / hard_deny / environment buckets.
- `--bare` is a documented minimal mode that skips hooks, LSP, plugin sync,
  attribution, auto-memory, background prefetches, keychain reads, and CLAUDE.md
  auto-discovery while still allowing explicit context via flags.
- Background agents are now a substantial CLI surface (`claude agents`), with
  JSON listing, dispatch defaults, worktree/tmux support, strict MCP config, and
  preserved model/effort/permission/plugin settings.
- Session recap is a literal feature (`/recap`, `awaySummaryEnabled`, and
  `CLAUDE_CODE_ENABLE_AWAY_SUMMARY`): it runs a one-turn no-tools
  `away_summary` fork from saved cache-safe params; the automatic path appends a
  short system status message rather than compacting/replacing history.

## Axis checklist

| Axis | Leaf | Status | Confidence | Notes |
|---|---|---|---|---|
| Transport & flags | [claude-transport](claude-transport.md) | enriched | high | beta inventory mined; per-flag header map open |
| Robustness | [claude-robustness](claude-robustness.md) | enriched | high | CC idioms catalogued from the api-robustness mine |
| Compaction | [claude-compaction](claude-compaction.md) | enriched | mixed | two-prompt structure (high); threshold math (medium) |
| Agent loop | [claude-agent-loop](claude-agent-loop.md) | enriched | mixed | parallel-tool + stop reasons observed; internals open |
| Context management | [claude-context-management](claude-context-management.md) | enriched | high | injections/reminders observed first-hand |
| Built-in tools | [claude-builtin-tools](claude-builtin-tools.md) | enriched | high | inventory + steering language observed first-hand |
| MCP tooling | [claude-mcp](claude-mcp.md) | enriched | high | deferred tiering / ToolSearch observed first-hand |
| Subagents | [claude-subagents](claude-subagents.md) | enriched | high | Agent/Task surface observed first-hand |
| Hooks | [claude-hooks](claude-hooks.md) | enriched | high | event catalogue, return fields, managed/plugin hooks mined |
| Skills | [claude-skills](claude-skills.md) | enriched | high | discovery + invocation + progressive disclosure observed |

## Next on this subject

- Promote `enriched` leaves to `verified` by feeding the axis convergence/
  divergence syntheses (per [charter §9](../harness-tracks.md#9-researcher-contract-how-to-pick-up-a-leaf)).
- String-mine the binary for the items each leaf marks **Open** (per-flag header
  values, compaction threshold call graph, subagent queueing/concurrency).
- On the next claude release, create `claude-<next>.md` with
  `supersedes: claude-2.1.160.md` and re-mine only the changed axes.

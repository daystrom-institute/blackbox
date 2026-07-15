---
title: "Codex — 0.136.0 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: codex
version: "0.136.0"
platform: linux-x86_64
captured: "2026-06-02"
supersedes: null
status: enriched
topic:
  - harness
  - codex
brief: "Historical Codex 0.136.0 source snapshot captured on 2026-06-02. Its findings were enriched in the original mining pass and are superseded for current Codex behavior by the main@8aae858958 snapshot."
---

# Codex - 0.136.0 (snapshot)

> **Historical snapshot.** Superseded by
> [Codex main@8aae858958](codex-main-8aae858958.md). This remains the temporal
> anchor for the original 2026-06-02 mining pass.

## Provenance

- **Subject:** Codex.
- **Version:** 0.136.0.
- **Backend / transport:** OpenAI Responses.
- **Note:** Distinct path from Brodex (codex CLI vs bro-harness/Responses); see PROJECT.md provider routing.
- **Source / extraction:** direct source reading from `~/repos/codex/codex-rs`.

## Update (2026-06-02 · mining pass)

All 15 axis cells for Codex were **enriched** (`confidence: high`). The 5
governance-axis cells came from the codex-lens discovery mine (the pass that
surfaced those axes); the 10 base-axis cells (transport…skills) were filled by a
full source mining pass over `codex-rs` (DeepSeek + GLM bros, 2026-06-02). Codex
is now the deepest-mined subject — fitting, since it's the discovery lens.
Per-cell frontmatter is authoritative; the table below is not re-statused.

## Axis checklist

| Axis | Leaf | Status | Confidence |
|---|---|---|---|
| Transport & Feature Flags | [codex-transport](codex-transport.md) | enriched | high |
| Robustness | [codex-robustness](codex-robustness.md) | enriched | high |
| Compaction | [codex-compaction](codex-compaction.md) | enriched | high |
| Session Lifecycle & History | [codex-session-lifecycle](codex-session-lifecycle.md) | enriched | high |
| Agent Loop | [codex-agent-loop](codex-agent-loop.md) | enriched | high |
| Context Management | [codex-context-management](codex-context-management.md) | enriched | high |
| Planning & Goal State | [codex-planning-goals](codex-planning-goals.md) | enriched | high |
| Built-in Tools | [codex-builtin-tools](codex-builtin-tools.md) | enriched | high |
| MCP Tooling | [codex-mcp](codex-mcp.md) | enriched | high |
| Subagents | [codex-subagents](codex-subagents.md) | enriched | high |
| Hooks | [codex-hooks](codex-hooks.md) | enriched | high |
| Skills | [codex-skills](codex-skills.md) | enriched | high |
| Privilege, Sandboxing & Approvals | [codex-privilege-approvals](codex-privilege-approvals.md) | enriched | high |
| Memory & Persistence | [codex-memory-persistence](codex-memory-persistence.md) | enriched | high |
| Modes, Personas & Roles | [codex-modes-personas](codex-modes-personas.md) | enriched | high |

The metatools axis was created later and originally pointed at the skills cell.
It receives a dedicated Codex finding in the superseding snapshot.

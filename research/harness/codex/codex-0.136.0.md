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
brief: "STUB snapshot for Codex 0.136.0: the temporal anchor and per-axis checklist for this subject. Provenance not yet recorded and no axis mined; pick up per the harness charter. Backend/transport: OpenAI Responses."
---

# Codex — 0.136.0 (snapshot)

> **Status: stub.** Provenance not yet recorded; no axis mined. This is the
> temporal anchor for the `codex` subject at 0.136.0. See the
> [charter](../harness-tracks.md) for the ontology and pickup contract.

## Provenance

- **Subject:** Codex.
- **Version:** 0.136.0.
- **Backend / transport:** OpenAI Responses.
- **Note:** Distinct path from Brodex (codex CLI vs bro-harness/Responses); see PROJECT.md provider routing.
- **Source / extraction:** <!-- TODO(mine): record binary or source path + extraction method (open-source read vs strings-mine) before mining any leaf. -->

## Update (2026-06-02 · mining pass)

All 15 axis cells for codex are now **enriched** (`confidence: high`). The 5
governance-axis cells came from the codex-lens discovery mine (the pass that
surfaced those axes); the 10 base-axis cells (transport…skills) were filled by a
full source mining pass over `codex-rs` (DeepSeek + GLM bros, 2026-06-02). Codex
is now the deepest-mined subject — fitting, since it's the discovery lens.
Per-cell frontmatter is authoritative; the table below is not re-statused.

## Axis checklist

| Axis | Leaf | Status | Confidence |
|---|---|---|---|
| Transport & Feature Flags | [codex-transport](codex-transport.md) | stub | unknown |
| Robustness | [codex-robustness](codex-robustness.md) | stub | unknown |
| Compaction | [codex-compaction](codex-compaction.md) | stub | unknown |
| Agent Loop | [codex-agent-loop](codex-agent-loop.md) | stub | unknown |
| Context Management | [codex-context-management](codex-context-management.md) | stub | unknown |
| Built-in Tools | [codex-builtin-tools](codex-builtin-tools.md) | stub | unknown |
| MCP Tooling | [codex-mcp](codex-mcp.md) | stub | unknown |
| Subagents | [codex-subagents](codex-subagents.md) | stub | unknown |
| Hooks | [codex-hooks](codex-hooks.md) | stub | unknown |
| Skills | [codex-skills](codex-skills.md) | stub | unknown |

## Next on this subject

- Record provenance above, then pick up leaves per
  [charter §9](../harness-tracks.md#9-researcher-contract-how-to-pick-up-a-leaf).
- Read the [claude exemplar snapshot](../claude/claude-2.1.160.md) for the
  target shape and tone.

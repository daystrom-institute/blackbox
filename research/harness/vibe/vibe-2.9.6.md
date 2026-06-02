---
title: "Mistral Vibe — 2.9.6 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: vibe
version: "2.9.6"
platform: linux-x86_64
captured: "2026-06-02"
supersedes: null
status: enriched
topic:
  - harness
  - vibe
brief: "Point-in-time snapshot for Mistral Vibe 2.9.6 — a minimal CLI coding agent. The most directly-mineable subject: OPEN-SOURCE Python at ~/repos/mistral-vibe (read the source, no binary strings needed). Records provenance and recon-level signals from the README that already triangulate several axes. Findings are stubs."
---

# Mistral Vibe — 2.9.6 (snapshot)

> **Most directly-mineable subject.** Vibe is open-source Python — mine the
> *source* at `~/repos/mistral-vibe` directly (no `strings` step). Provenance
> below is verified; the "recon signals" are README-stated (confidence medium),
> not yet code-confirmed.

## Provenance

- **Subject:** Mistral Vibe (`vibe`) — Mistral AI's minimal CLI coding agent.
- **Version:** 2.9.6 (`vibe --version`).
- **Source:** **open source, Python 3.12+** at `~/repos/mistral-vibe` — mine
  `vibe/` (`acp`, `cli`, `core`, `setup`) directly. Packaging: `pyinstaller`,
  Zed integration (`distribution/zed`), an ACP spec (`vibe-acp.spec`).
- **Installed:** `~/.local/bin/vibe` → uv tool at
  `~/.local/share/uv/tools/mistral-vibe`.
- **Config:** `~/.vibe/` (`prompts/`, `skills/`); project + user `AGENTS.md`.
- **Protocol note:** speaks **ACP** (Agent Client Protocol) — relevant to the
  transport/surfaces axes.

## Recon signals (pre-mine — confidence medium, README-stated, NOT code-confirmed)

- **builtin-tools** — `read_file`, `write_file`, `search_replace`, `bash`,
  `grep`, todo, `ask_user_question`, task delegation. *(ask_user_question
  confirms the builtin-tools elicitation extension)*
- **context-management** — `AGENTS.md` (project + user); replaceable system
  prompts in `~/.vibe/prompts/`.
- **agent-loop** — interactive + programmatic (`--prompt`, `--max-turns`,
  `--max-price`, `--max-tokens`). *(`--max-price`/`--max-turns` are budget knobs
  — cross-ref planning-goals †)*
- **mcp** — `config.toml`; HTTP / streamable-HTTP / stdio transports.
- **skills** — agent-skills spec; discovered from `.vibe/skills/`,
  `~/.vibe/skills/`, custom paths.
- **privilege-approvals †** — built-in agents with permission models:
  `default` (approval required), `plan` (read-only auto-approve), `accept-edits`
  (auto-approve edits), `auto-approve` (all). *(confirms axis)*
- **session-lifecycle †** — `--continue`, `--resume SESSION_ID`, logging.
  *(confirms axis)*
- **modes-personas †** — agent profiles (`default`/`plan`/`accept-edits`/
  `auto-approve`) + custom agent configs; voice mode (experimental).
  *(confirms axis)*

## Update (2026-06-02 · mining pass)

All 15 axis cells for Vibe are now **enriched** (`confidence: high`) — mined from
the open-source Python at `~/repos/mistral-vibe` by GLM-5.1 bros (file:line +
quotes). Vibe is notable as the **negative case** on two governance axes: it has
**no durable goal/memory**, and it **does not declare the permission envelope to
the model** (enforced externally). Per-cell frontmatter is authoritative; the
table below predates this pass and is not re-statused.

## Axis checklist

| Axis | Leaf | Status | Confidence |
|---|---|---|---|
| Transport & Feature Flags | [vibe-transport](vibe-transport.md) | stub | unknown |
| Robustness | [vibe-robustness](vibe-robustness.md) | stub | unknown |
| Compaction | [vibe-compaction](vibe-compaction.md) | stub | unknown |
| Agent Loop | [vibe-agent-loop](vibe-agent-loop.md) | stub | unknown |
| Context Management | [vibe-context-management](vibe-context-management.md) | stub | unknown |
| Built-in Tools | [vibe-builtin-tools](vibe-builtin-tools.md) | stub | unknown |
| MCP Tooling | [vibe-mcp](vibe-mcp.md) | stub | unknown |
| Subagents | [vibe-subagents](vibe-subagents.md) | stub | unknown |
| Hooks | [vibe-hooks](vibe-hooks.md) | stub | unknown |
| Skills | [vibe-skills](vibe-skills.md) | stub | unknown |

(New-axis cells — the five † axes — are seeded when mined, per the charter.)

## Next on this subject

- Mine the source at `~/repos/mistral-vibe/vibe/` directly: system prompt
  assembly, the tool definitions + their docstrings/steering language, the
  agent/permission-model code, session + skills handling. Confirm recon signals.
- Read the [claude exemplar snapshot](../claude/claude-2.1.160.md) for target
  shape/tone.

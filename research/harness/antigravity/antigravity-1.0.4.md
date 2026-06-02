---
title: "Antigravity CLI (agy) — 1.0.4 (snapshot)"
kind: research-subject
corpus: blackbox-research
track: harness
harness: antigravity
version: "1.0.4"
platform: linux-x86_64
captured: "2026-06-02"
supersedes: null
replaces: gemini
status: enriched
topic:
  - harness
  - antigravity
brief: "Point-in-time snapshot for Antigravity CLI (agy) 1.0.4 — Google's terminal coding agent, replacing the deprecated Gemini CLI subject. Records provenance (a mineable Go-native binary) and recon-level signals from the public README + CHANGELOG that already triangulate several axes, ahead of the deep string-mine. Findings are stubs."
---

# Antigravity CLI (agy) — 1.0.4 (snapshot)

> **Replaces the deprecated Gemini CLI subject.** Gemini CLI is deprecated;
> `agy` is Google's terminal coding agent going forward. It still uses the
> `~/.gemini/` config namespace (the deprecation lineage). The former `gemini/`
> folder (empty stubs) was removed.
>
> **This snapshot is recon-level, not mined.** Provenance below is verified;
> the "recon signals" are read from the public README + CHANGELOG (confidence
> low/medium) and are *not* code-confirmed. The deep mine is open per leaf.

## Provenance

- **Subject:** Antigravity CLI (`agy`) — Google's terminal coding agent; shares
  a "Core Agent Engine" with the Antigravity 2.0 IDE (session export bridges
  CLI↔IDE; settings/permissions sync).
- **Version:** 1.0.4 (`agy --version`).
- **Binary:** `~/.local/bin/agy` — ELF 64-bit LSB pie, Go-native, ~177 MB,
  ~495K printable strings → **mineable via `strings -n8` + grep** (same approach
  as the claude binary; closed source). Self-updates in the background.
- **Config namespace:** `~/.gemini/` — `settings.json`, `hooks/`, `GEMINI.md`,
  `projects.json`, `oauth_creds.json`, MCP config, `rules.json`. (Inherited from
  Gemini CLI.)
- **Install:** `curl -fsSL https://antigravity.google/cli/install.sh | bash`
  (SHA512-verified flat native build from Google's auto-updater service). Docs
  repo cloned to `~/repos/antigravity-cli` (README + CHANGELOG +
  examples/{statusline,title}); the repo is docs-only, not source.
- **Sibling:** Antigravity 2.0 IDE (`/usr/bin/antigravity`, 1.107.0) — same
  engine, GUI surface.
- **Auth:** system keyring → Google Sign-In; SSH-aware. **Telemetry:**
  Interactions data collected by Google by default (opt-out in settings).

## Recon signals (pre-mine — confidence low/medium, NOT code-confirmed)

From README + CHANGELOG. Each is a hypothesis to confirm in the mine; several
already triangulate the codex-derived axes on a third harness:

- **privilege-approvals †** — `proceed-in-sandbox` tool permission mode
  (auto-approves commands inside the sandbox, prompts on bypass); "Sandbox Mode";
  `rules.json` allowlists/exclusions. *(confirms axis)*
- **session-lifecycle †** — SQLite (`.db`) conversation persistence; `/resume`;
  session export to the IDE; import from 2.0. *(confirms axis)*
- **subagents** — "specialized agents"; a 60s interaction timeout scoped to
  subagents.
- **mcp** — custom MCP servers (`mcp_config.json`), parallelized init, TUI
  enable/disable.
- **skills** — skill-derived slash commands; **plugin** discovery for skills +
  agents (plugins install to `~/.gemini/config/`).
- **builtin-tools** — `AskQuestion` structured elicitation (options, write-in
  values, multi-question dialogs). *(confirms the builtin-tools I/O-contract
  extension)*
- **hooks** — statusline + title hooks: a JSON payload of `agent_state` / `vcs` /
  context-window usage / terminal dims is piped to a shell script on stdin.
- **context-management** — `.md` rule-file discovery via `rules.json`; `GEMINI.md`
  overlay.
- **modes-personas †** — "Review Mode" (statusline state).
- **transport** — G1 credits / `/usage` / `/quota` quota surfaces.

## Update (2026-06-02 · mining pass)

All 15 axis cells for agy are now populated — mined from the Go binary
(`strings`) + `~/.gemini/` config + docs by DeepSeek-v4-pro bros. Confidence is
**medium** for server-side behaviors (agy is a thin gRPC client to the cortex
engine) and **high** for verbatim binary strings + live config (e.g. the
two-tier `run_command` sandbox prompt, the 7 hook types, the SQLite session
transition). Per-cell frontmatter is authoritative; the table below predates this
pass and is not re-statused.

## Axis checklist

| Axis | Leaf | Status | Confidence |
|---|---|---|---|
| Transport & Feature Flags | [antigravity-transport](antigravity-transport.md) | stub | unknown |
| Robustness | [antigravity-robustness](antigravity-robustness.md) | stub | unknown |
| Compaction | [antigravity-compaction](antigravity-compaction.md) | stub | unknown |
| Agent Loop | [antigravity-agent-loop](antigravity-agent-loop.md) | stub | unknown |
| Context Management | [antigravity-context-management](antigravity-context-management.md) | stub | unknown |
| Built-in Tools | [antigravity-builtin-tools](antigravity-builtin-tools.md) | stub | unknown |
| MCP Tooling | [antigravity-mcp](antigravity-mcp.md) | stub | unknown |
| Subagents | [antigravity-subagents](antigravity-subagents.md) | stub | unknown |
| Hooks | [antigravity-hooks](antigravity-hooks.md) | stub | unknown |
| Skills | [antigravity-skills](antigravity-skills.md) | stub | unknown |

(New-axis cells — the five † axes — are seeded when mined, per the charter.)

## Next on this subject

- String-mine `~/.local/bin/agy` (`strings -n8` + grep) for system prompts,
  tooldoc language, `<system-reminder>`-equivalents, and the sandbox/approval
  messaging. Confirm the recon signals above.
- Inspect `~/.gemini/` (settings.json, hooks/, rules.json, mcp_config) for the
  config-side agent-facing surfaces.
- Read the [claude exemplar snapshot](../claude/claude-2.1.160.md) for target
  shape/tone.

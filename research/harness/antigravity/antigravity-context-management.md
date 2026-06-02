---
title: "Antigravity · Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: context-management
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - context-management
brief: "agy context: ~/.gemini/GEMINI.md overlay; CustomAgentSystemPromptConfig with selective IncludeSections; a PLUGIN BRIDGE that imports both Gemini CLI and Claude Code configs (skills/agents/commands/hooks/MCP); MemoryConfig.AddUserMemoriesToSystemPrompt; rules.json for .md rule-file discovery (allowlist/exclusion)."
---

# Antigravity · Context Management

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Context Management](../context-management.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Reads `~/.gemini/GEMINI.md` as a system-prompt overlay. `CustomAgentSystemPromptConfig.GetIncludeSections` selectively injects sections. A **plugin bridge** imports configs from *both* Gemini CLI (`GeminiCLIImporter`) and **Claude Code** (`ClaudeCodeImporter`: `stageClaudeMCPServers`, `stageClaudeCommands`), staging skills/agents/commands/hooks/MCP. `MemoryConfig.GetAddUserMemoriesToSystemPrompt` injects user memories. `rules.json` carries allowlist/exclusion rules for `.md` rule-file discovery (v1.0.4 fixed it being silently ignored).

**Evidence.**
- `CustomAgentConfig{GetSystemPromptSections,GetSystemPromptConfig}`
- `cli/plugins/claude.ClaudeCodeImporter.Import` + `stageClaudeMCPServers`
- `MemoryConfig{GetAddUserMemoriesToSystemPrompt}`

**Vs the axis.** Confirms overlay (GEMINI.md) + rule-file discovery + memory injection. **Notable:** the cross-harness plugin bridge (importing Claude Code's surfaces) is an interop pattern no other subject has.

## Open
<!-- rules.json schema (absent on this host); section-selection logic. -->

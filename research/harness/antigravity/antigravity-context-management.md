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
confidence: high
topic:
  - harness
  - antigravity
  - context-management
brief: "SDK context controls include plain string system instructions, TemplatedSystemInstructions identity/section appends, CustomSystemInstructions replacement, capability filtering that removes disabled tools from model context, response_schema, workspaces, skills_paths, and app_data_dir/save_dir separation. CLI strings add GEMINI.md/rules/plugin bridge signals."
---

# Antigravity · Context Management

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Context Management](../context-management.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Reads `~/.gemini/GEMINI.md` as a system-prompt overlay. `CustomAgentSystemPromptConfig.GetIncludeSections` selectively injects sections. A **plugin bridge** imports configs from *both* Gemini CLI (`GeminiCLIImporter`) and **Claude Code** (`ClaudeCodeImporter`: `stageClaudeMCPServers`, `stageClaudeCommands`), staging skills/agents/commands/hooks/MCP. `MemoryConfig.GetAddUserMemoriesToSystemPrompt` injects user memories. `rules.json` carries allowlist/exclusion rules for `.md` rule-file discovery (v1.0.4 fixed it being silently ignored).

**Evidence.**
- `CustomAgentConfig{GetSystemPromptSections,GetSystemPromptConfig}`
- `cli/plugins/claude.ClaudeCodeImporter.Import` + `stageClaudeMCPServers`
- `MemoryConfig{GetAddUserMemoriesToSystemPrompt}`

**Vs the axis.** Confirms overlay (GEMINI.md) + rule-file discovery + memory injection. **Notable:** the cross-harness plugin bridge (importing Claude Code's surfaces) is an interop pattern no other subject has.

## SDK/local harness update (2026-06-02)

The SDK makes context assembly configurable without exposing private prompt prose. system_instructions can be a plain string, TemplatedSystemInstructions, or CustomSystemInstructions. The templated form appends identity and structured sections to the default harness prompt; the custom form replaces defaults. This is a clearer persona/context API than the earlier CLI-string evidence alone.

Tool context is controlled separately from policy. CapabilitiesConfig enabled_tools/disabled_tools removes tools from model context, while policies leave tools visible and reject/ask at runtime. MCP server configs have the same enabled_tools/disabled_tools filter, so both builtins and server tools can be pruned before prompt assembly.

Other context-bearing fields include workspaces, response_schema, skills_paths, save_dir, and app_data_dir. Binary strings still show prompt template paths for artifacts, planning-mode artifacts, function-call formatting, knowledge items, persistent context, plugins, skills, and slash commands. Those names are useful evidence of prompt sections, but the corpus should not quote proprietary prompt text.

## Open
<!-- rules.json schema (absent on this host); section-selection logic. -->

---
title: "Antigravity · Built-in Tools"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: builtin-tools
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - builtin-tools
brief: "SDK source gives the builtin list: list_directory, search_directory, find_file, view_file, create_file, edit_file, run_command, ask_question, start_subagent, generate_image, finish. Tool sets include read_only, nondestructive, and file_tools; disabled tools are removed from model context while policy-denied tools remain visible but reject at runtime."
---

# Antigravity · Built-in Tools

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Built-in Tools](../builtin-tools.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Tools are **server-side**; the binary carries config protos: `GrepToolConfig`, `MqueryToolConfig` (semantic codebase search), `ReplaceContentToolConfig`, `ViewFileToolConfig`, `MemoryToolConfig`, `run_command` (sandboxed — see privilege), `AntigravityBrowserToolConfig` (screenshots, click-by-pixel), `InvokeSubagentToolConfig`. `AskQuestion` provides multi-question structured elicitation (v1.0.3 memorizes selected options/write-ins). The `run_command` **sandbox prompt is embedded verbatim** (high-conf), but otherwise there are **no generic tooldoc steering strings** ("Use this tool" / "do not") in the binary — the server owns descriptions.

**Evidence.**
- `MqueryToolConfig{GetDisableSemanticCodebaseSearch}`; `AntigravityBrowserToolConfig{GetClickBrowserPixel}`
- CHANGELOG v1.0.3: "AskQuestion … memorizes selected options, write-in values"
- `run_command` sandbox prompt string (binary ~line 163965)

**Vs the axis.** Confirms the elicitation extension (`AskQuestion`) + a **semantic code-search tool** (mquery) most others lack. **Divergence:** tooldoc steering language is *not* in the client — unminable here (server-owned), unlike claude/vibe.

## SDK/local harness update (2026-06-02)

The SDK's BuiltinTools enum names the stable public tool surface: list_directory, search_directory, find_file, view_file, create_file, edit_file, run_command, ask_question, start_subagent, generate_image, and finish. It also defines useful capability bundles. read_only exposes list/search/find/view plus finish. nondestructive includes create/edit/ask_question/start_subagent/generate_image/finish but excludes run_command. file_tools isolates view/create/edit.

CapabilitiesConfig separates visibility from runtime policy. enabled_tools and disabled_tools prune tools from the model context, which is a real context-bloat control. Policies do not prune; they leave the tool visible and reject or ask at execution time. This distinction matters for Blackbox design because Antigravity treats context minimization and approval policy as separate levers.

The generated localharness proto maps public builtins to harness-side fields: create_file, edit_file, find_file, list_directory, run_command, search_directory, view_file, invoke_subagent, generate_image, and finish. Tool calls and results are first-class messages. Custom Python callables are converted to FunctionDeclaration-backed Tool protos, then handled by the same ToolRunner as builtins/MCP.

## Open
<!-- Server-side tool descriptions/steering (not client-visible); full tool inventory. -->

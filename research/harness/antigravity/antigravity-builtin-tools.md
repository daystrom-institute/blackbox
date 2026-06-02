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
confidence: medium
topic:
  - harness
  - antigravity
  - builtin-tools
brief: "agy tools are server-side (cortex-managed) — config protos only in the binary: GrepToolConfig, MqueryToolConfig (semantic codebase search), ReplaceContentToolConfig, ViewFileToolConfig, MemoryToolConfig, run_command (sandboxed), browser tools, InvokeSubagentToolConfig. AskQuestion structured elicitation. The run_command sandbox prompt IS embedded verbatim; no general tooldoc steering strings."
---

# Antigravity · Built-in Tools

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Built-in Tools](../builtin-tools.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Tools are **server-side**; the binary carries config protos: `GrepToolConfig`, `MqueryToolConfig` (semantic codebase search), `ReplaceContentToolConfig`, `ViewFileToolConfig`, `MemoryToolConfig`, `run_command` (sandboxed — see privilege), `AntigravityBrowserToolConfig` (screenshots, click-by-pixel), `InvokeSubagentToolConfig`. `AskQuestion` provides multi-question structured elicitation (v1.0.3 memorizes selected options/write-ins). The `run_command` **sandbox prompt is embedded verbatim** (high-conf), but otherwise there are **no generic tooldoc steering strings** ("Use this tool" / "do not") in the binary — the server owns descriptions.

**Evidence.**
- `MqueryToolConfig{GetDisableSemanticCodebaseSearch}`; `AntigravityBrowserToolConfig{GetClickBrowserPixel}`
- CHANGELOG v1.0.3: "AskQuestion … memorizes selected options, write-in values"
- `run_command` sandbox prompt string (binary ~line 163965)

**Vs the axis.** Confirms the elicitation extension (`AskQuestion`) + a **semantic code-search tool** (mquery) most others lack. **Divergence:** tooldoc steering language is *not* in the client — unminable here (server-owned), unlike claude/vibe.

## Open
<!-- Server-side tool descriptions/steering (not client-visible); full tool inventory. -->

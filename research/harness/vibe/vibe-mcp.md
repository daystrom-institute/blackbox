---
title: "Vibe · MCP Tooling"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: mcp
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - mcp
brief: "Vibe MCP: three transports (http / streamable-http / stdio) as a discriminated union in config; MCPRegistry fingerprints server config (SHA-256) and caches discovery; remote tools wrapped as dynamic proxy classes; per-server prompt/disable/disabled_tools/timeouts; sampling pass-through (sampling_enabled) lets MCP servers call back into the LLM."
---

# Vibe · MCP Tooling

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [MCP Tooling](../mcp.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** `VibeConfig.mcp_servers` is a discriminated union over `transport`: `MCPHttp` / `MCPStreamableHttp` / `MCPStdio`. `MCPRegistry` fingerprints each server's config by SHA-256, caches the tool list, and on miss calls `list_tools_http`/`list_tools_stdio` (Python `mcp` SDK). Each remote tool becomes a dynamically-created proxy class extending `MCPTool`, registered into `ToolManager` alongside builtins. Per-server: `prompt` (usage hint appended to tool desc), `disabled`, `disabled_tools`, `startup_timeout_sec`, `tool_timeout_sec`, and **`sampling_enabled`** (lets the MCP server make LLM calls back through the host). Mistral connectors are a parallel system.

**Evidence.**
- `vibe/core/config/_settings.py:280` — MCP discriminated union (`transport` field)
- `vibe/core/tools/mcp/registry.py:1` — fingerprinted cache + discovery
- `vibe/core/config/_settings.py:257` — `sampling_enabled: bool = True`

**Vs the axis.** Confirms 3-transport MCP. **Divergence:** vibe does **not** use deferred-tiering/tool-search (unlike Claude/codex) — all configured MCP tools are eagerly proxied. `sampling_enabled` (server→LLM callback) is a surface our axis didn't list.

## Open
<!-- Does eager proxying bloat context at high tool counts? confirm no name-only deferral path. -->

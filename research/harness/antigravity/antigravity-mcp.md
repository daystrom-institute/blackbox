---
title: "Antigravity · MCP Tooling"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: mcp
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - mcp
brief: "agy MCP: config at ~/.gemini/antigravity/mcp_config.json (+ legacy settings.json mcpServers); McpServerConfig local(command/args/env) + remote(url/headers); full resource/prompt surface; PER-TOOL disabling (disabledTools); team AllowMcpServers; marketplace McpServerTemplate; Claude Code MCP import bridge; parallelized init (v1.0.4)."
---

# Antigravity · MCP Tooling

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [MCP Tooling](../mcp.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Config lives in `~/.gemini/antigravity/mcp_config.json` (and legacy `~/.gemini/settings.json` `mcpServers` — a v1.0.3 migration target). `McpLocalServer{command,args,env}` and `McpRemoteServer{type,url,headers}`; full `McpResourceItem`/`McpPromptMessage`/`McpPromptArgument` surface. **Per-tool disabling** via `disabledTools` (live config disables 63 tools on the `blackbox` server; `daystrom` fully disabled). Team-level `AllowMcpServers`; marketplace `McpServerTemplate`; Claude Code MCP import via the plugin bridge; parallelized server init (v1.0.4).

**Evidence.**
- `~/.gemini/antigravity/mcp_config.json`: `{"blackbox":{"serverUrl":…,"disabledTools":[…63…]},"daystrom":{"disabled":true}}`
- `McpRemoteServer{GetType,GetUrl,GetHeaders}`; CHANGELOG v1.0.4 "Parallelized the MCP server initialization"

**Vs the axis.** Confirms full MCP (resources+prompts), and adds **per-tool disabling** + team allowlists (finer than Claude's native MCP). No client-side tool-search/deferral observed (server dispatches).

## Open
<!-- Whether deferred tiering exists server-side; tool_search vs MCP tools/list. -->

---
title: "Antigravity - MCP Tooling"
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
brief: "SDK supports MCP stdio, SSE, and streamable HTTP servers with per-server enabled_tools/disabled_tools filtering; McpBridge connects servers and extends the ToolRunner. Current host has ~/.gemini/config/mcp_config.json present but empty, so older populated-config claims are not live-confirmed here."
---

# Antigravity - MCP Tooling

> Evidence: public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f, installed agy 1.0.4 binary strings/changelog, and current ~/.gemini host state. SDK claims are high confidence for the SDK/localharness surface; CLI/cortex claims remain medium unless backed by live state or verbatim binary strings.
See axis: [MCP Tooling](../mcp.md) - snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

## Finding

The SDK exposes MCP as a first-class tool source. It defines McpStdioServer, McpSseServer, and McpStreamableHttpServer. Each server config accepts enabled_tools and disabled_tools, so MCP tool visibility can be narrowed before tools enter model context. During Agent startup, McpBridge connects the configured servers and extends the ToolRunner with server-provided tools.

The SDK references cover stdio and SSE. The source type surface also includes streamable HTTP. Policy helpers are overloaded for MCP server config objects, letting the same allow/deny/ask policy layer apply to MCP tools after context-level filtering has already happened.

CLI evidence is thinner. Binary strings and changelog text show McpRemoteServer accessors, MCP resource/prompt vocabulary, import/migration paths, team allowlist concepts, marketplace template vocabulary, and v1.0.4 parallelized MCP server initialization. On this host the only live MCP config found is ~/.gemini/config/mcp_config.json, and it is zero bytes. Earlier claims about a populated ~/.gemini/antigravity/mcp_config.json are not current live evidence here.

## Design Takeaways

- Antigravity separates MCP tool selection from policy. enabled_tools/disabled_tools trim context; policy decides whether a visible tool call is allowed.
- MCP tools are not a side channel in the SDK. They are merged into the same ToolRunner path as builtins and Python callables.
- The SDK offers no observed deferred MCP discovery/tool-search layer. If cortex does server-side tiering, that remains unconfirmed.

## Open

- Populated CLI MCP schema and migration behavior.
- Whether CLI/cortex defers MCP tools or sends all enabled server tools to the model.
- Whether streamable HTTP is exposed in standalone agy config or only in the SDK type layer.

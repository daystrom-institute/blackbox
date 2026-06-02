---
title: "Codex · MCP Tooling"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: mcp
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - mcp
brief: "Codex MCP: deferred tiering gated at DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD=100 (or ToolSearchAlwaysDeferMcpTools) — at/above, ALL tools deferred; below, all direct. tool_search runs BM25 over concatenated metadata (name/server/title/desc/schema-props). Naming mcp__server__tool. Multi-layer approval (hooks → guardian → user elicitation w/ arg-hash session persistence). Dynamic (app-server) tools share the same deferred/search pipeline."
---

# Codex · MCP Tooling

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [MCP Tooling](../mcp.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** `build_mcp_tool_exposure()` counts visible MCP tools; if `ToolSearchAlwaysDeferMcpTools` OR count `>= DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD (100)`, **all** go to the deferred tier (zero direct); else all direct. `tool_search` does **BM25** over a corpus built from `flat_tool_name, callable_name, name, server_name, title, description, connector_name, namespace_description, plugin_display_names, schema_properties` (not vector). Naming: `mcp__server__tool` (`__` delimiter, `mcp__` legacy prefix via `ensure_mcp_prefix`). Approval is multi-layer: `PermissionRequest` **hook** can short-circuit → guardian review → user elicitation with "Allow and don't ask me again" (per-session key = server+tool+arg-hash; promotable to persistent). Dynamic app-server tools use the same `ToolExposure` Direct/Deferred + search pipeline.

**Evidence.**
- `core/src/mcp_tool_exposure.rs:14-47` — threshold-gated deferral
- `core/src/tools/handlers/tool_search_spec.rs:18` + `mcp.rs:260-308` — BM25 `build_mcp_search_text`
- `core/src/mcp_tool_call.rs:960,1137` — approval decision + arg-hash key

**Vs the axis.** Confirms deferred-tiering (capacity threshold + BM25) — the cross-harness anti-bloat invariant, matching Claude. Extends with hook-short-circuit approval + dynamic-tool unification.

## Open
<!-- Whether tool_search-loaded tools persist across turns; connector allowlist mechanics. -->

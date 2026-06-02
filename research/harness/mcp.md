---
title: "Axis: MCP Tooling"
kind: research-axis
corpus: blackbox-research
track: harness
axis: mcp
topic:
  - harness
  - mcp
brief: "Cross-harness invariant model for the MCP-tooling axis: how a harness integrates Model Context Protocol servers — transport (stdio/SSE/HTTP), progress/streaming, and especially tool search / deferred loading / discovery (surface tools by name, load schemas on demand). The deferred-tiering pattern is the anti-bloat keystone. Synthesis of the per-subject MCP cells."
---

# Axis: MCP Tooling

> **Scope.** Integration of external MCP servers — distinct from the harness's
> own [built-in tools](builtin-tools.md). Covers MCP transport, streaming/progress,
> and the discovery/loading model. The headline question: how does a harness
> expose dozens-to-hundreds of MCP tools without paying their full schema cost
> every turn?

## The dimension

MCP is where context bloat goes to win unless the harness is disciplined. A
server can expose 200 tools; naively, every tool's full JSON schema rides in
context every turn. The modern answer is **deferred tiering**: surface tools by
*name only* and load each tool's schema on demand via a search/fetch tool. (This
very session runs on exactly that mechanism.) Capturing how each harness does
discovery, deferral, and progress is the anti-bloat keystone.

## Questions a finding must answer

- **MCP transports.** stdio, SSE, streamable-HTTP? Connection lifecycle
  (per-session, pooled, reconnect)?
- **Streaming / progress.** Are MCP progress notifications surfaced to the agent
  or the user? Partial results?
- **Tool discovery.** How are MCP tools listed to the model — full schema up
  front, or names-only?
- **Deferred loading / tool search.** Is there a search/fetch step that loads
  schemas on demand? What is the query interface? How are loaded tools made
  callable?
- **Namespacing.** FQDN (`mcp__server__tool`) vs bare names? When is each used?
- **Server instructions.** Are MCP server-provided instructions injected? Where?
- **Auth / availability.** Interactive-auth servers in headless runs — present
  or absent?

## Convergence / divergence

| Subject | MCP transports | Discovery model | Deferred loading | Namespacing | Cell |
|---|---|---|---|---|---|
| Claude | stdio/SSE/HTTP | _TBD_ | tool-search (names→schema) | FQDN | [claude](claude/claude-mcp.md) |
| Codex | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [codex](codex/codex-mcp.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-mcp.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-mcp.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is names-only-then-fetch the convergent anti-bloat pattern, or Claude-specific?
- What is the query interface for tool search across harnesses (keyword? select-
  by-name? ranked?)?
- How is a just-loaded tool made callable mid-session without a reconnect?

## Codex-lens extensions

Deferred tiering is **confirmed cross-harness** (Claude + Codex both do
names-only + a search/fetch step) — a genuine invariant, not a Claude quirk. A
finding should also cover:

- **Capacity threshold** — deferral may be gated past a tool-count threshold
  (codex: >100), not always-on.
- **Ranking** — the search step may be BM25-ranked over deferred tool metadata.
- **Mid-session surface expansion** — the tool surface can *grow* mid-session: a
  skill/app mention can auto-install MCP dependencies, and a plugin `@mention`
  can activate a whole bundle (skills + MCP + apps + hooks) under one namespace.
  The model must not assume the tool list is fixed at session start.

## Feeds

`design/surfaces/mcp/` (the daemon MCP surface designs),
`design/bro-harness/backlog-transport-polish.md` (MCP connection pooling +
deferred-manifest trimming). bro-harness MCP injection: `configure_dispatch_mcp_env`.

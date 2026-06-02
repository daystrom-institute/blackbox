---
title: "Claude · MCP Tooling"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: mcp
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - mcp
brief: "How Claude Code 2.1.160 integrates MCP servers, with emphasis on the deferred-tiering anti-bloat pattern: MCP tools are surfaced by name only in a <system-reminder> manifest, their JSON schemas withheld until fetched via the ToolSearch tool (select-by-name or keyword/ranked queries), then become callable in-session. FQDN namespacing (mcp__server__tool). Backfilled from direct observation of the running harness, which runs on exactly this mechanism."
---

# Claude · MCP Tooling

> **Provenance.** Direct observation of the running 2.1.160 harness — this
> session operates on the deferred-tiering mechanism described here.
> **confidence: high.** See [snapshot](claude-2.1.160.md).

See the axis: [MCP Tooling](../mcp.md).

## Deferred tiering — the anti-bloat keystone (high)

Claude Code does **not** load every MCP tool's schema into context. Instead:

1. **Names-only manifest.** A `<system-reminder>` lists deferred tools by FQDN
   only: *"The following deferred tools are now available via ToolSearch. Their
   schemas are NOT loaded — calling them directly will fail with
   InputValidationError."* Hundreds of `mcp__blackbox__*` / `mcp__blackbox-ops__*`
   names appear at ~0 schema cost.
2. **On-demand schema load.** `ToolSearch` resolves names → full JSONSchema. Query
   forms observed:
   - `select:Read,Edit,Grep` — fetch exact tools by name.
   - `notebook jupyter` — keyword search, ranked, `max_results`-capped.
   - `+slack send` — require a term in the name, rank by the rest.
   The result is a `<functions>` block of full definitions, *"the same encoding
   as the tool list at the top of the prompt."*
3. **Callable after fetch.** Once a schema appears in the ToolSearch result, the
   tool is *"immediately callable exactly like any tool defined here"* — no
   reconnect.

This is the same progressive-disclosure lever as [skills](claude-skills.md),
applied to tools — the single most important pattern for "steer without bloat."

## Namespacing & server instructions (high)

- **FQDN** form `mcp__<server>__<tool>` (e.g. `mcp__blackbox__bbox_note`); two
  servers can expose the same logical tool under distinct prefixes
  (`mcp__blackbox__…` vs `mcp__blackbox-ops__…`).
- **Server instructions** are injected under an "MCP Server Instructions" heading
  with a short per-server blurb.
- **Auth caveat (medium).** Interactively-authenticated MCP servers (e.g.
  claude.ai connectors) may be absent in headless/cron runs — surfaced as a
  documented caveat.

## Open

<!-- TODO(mine): the ToolSearch ranking algorithm (BM25? embedding?); the exact
threshold/policy for which tools start deferred vs eagerly loaded; MCP transport
details (stdio/SSE/streamable-HTTP) and progress-notification surfacing; whether
loaded schemas persist across compaction. -->

## Feeds

`design/surfaces/mcp/`, `design/bro-harness/backlog-transport-polish.md`
(deferred-manifest trimming + MCP connection pooling). The deferred-tiering model
is the target for bro-harness's MCP surface.

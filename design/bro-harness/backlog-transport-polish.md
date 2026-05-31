---
title: "bro-harness transport & tool polish (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
brief: "Residual transport/tool polish for the shipped bro-harness: MCP connection pooling, codex_auth retry wrapping, deferred-manifest token trimming, optional client-side web_search (Brave) fallback, server result normalization, structured output, namespace isolation, in-process executor, and RTK-style per-command output compaction. None are correctness gaps — the harness is live end-to-end; these are latency/cost/robustness/extensibility upgrades."
---

# bro-harness transport & tool polish (backlog)

> **Provenance.** Extracted from [`anthropic-harness.md`](./anthropic-harness.md)
> "Open questions / later", enriched with the live item-5 residue from
> **thread-ca160aa2** ("bro-harness remaining work"). Items 1–4 of that thread
> (surface/client-filter split, allow/deny recursion guard, HTTP robustness with
> retry/backoff/Retry-After, resume + wire-contract test) and item 2 (SSE
> streaming on all three transports) are **done**; what follows is the residue.

The harness is built and live-verified end-to-end. Nothing here is a correctness
gap. These are independently pickup-able polish items; grab any one.

## Done-able now (low risk, clear shape)

- **MCP connection pooling.** Today `crates/bro-harness/src/mcp.rs` re-dials its
  MCP server on every tool call (`McpTool::call_inner` opens a fresh
  `StreamableHttpClientTransport` per dispatch; the module comment flags pooling
  as "a later optimization"). Hold a persistent per-server client keyed by URL,
  reused across calls for the session lifetime; fall back to a fresh dial on
  error. **Acceptance:** one dial per server per session (not per call) under a
  multi-MCP-tool transcript; no behavior change on dial failure.
- **Wrap `codex_auth` token refresh in the retry helper.** The OAuth refresh POST
  in `crates/bro-harness/src/transport/codex_auth.rs` runs outside the
  `http::send_with_retry` helper that items-4 added to the three transport
  clients, so a transient network blip during refresh fails hard. Route it
  through the same capped-backoff/Retry-After helper. **Acceptance:** a simulated
  transient failure on the refresh endpoint retries rather than aborting the
  dispatch.
- **Deferred-manifest token trimming.** The deferred-tooling manifest
  (`tool_search` tier) is emitted untrimmed. Trim the per-tool manifest text to
  a token budget so large MCP surfaces don't bloat the pinned manifest.
  **Acceptance:** manifest stays under a configurable token budget with a
  documented truncation rider when it would overflow.

## Extensibility / later

- **Client-side `web_search` fallback backend.** Only needed for providers
  without a server-side search tool; GLM and DeepSeek both have one, so this is
  not required for the current provider set. `crates/bro-tools/src/web.rs`
  deliberately omits `web_search` (provider-executed passthrough) and ships only
  `web_fetch`. If added: Brave (pg_recon's choice, paid key) vs. an alternative,
  pluggable behind a trait.
- **Server result normalization.** Whether to canonicalize GLM's
  `web_search_prime`/`tool_result` variant into the Anthropic
  `web_search_tool_result` shape inside the conversation, or relay verbatim.
  Verbatim is simpler and the model tolerated its own provider's shape; revisit
  only if a model gets confused by its own format.
- **Structured output.** Add `--output-schema` + forced `tool_choice` to the
  harness if/when an actor needs `StructuredOutput` from GLM/DeepSeek.
- **Reusing the harness for `Provider::Claude` itself.** Out of scope now, but
  the design is provider-generic — if the official CLI keeps drifting, route
  Claude through the same harness against the first-party endpoint.
- **Namespace isolation.** The daemon already sets cwd per task; PID/mount
  isolation (daystrom does `unshare`) belongs in the daemon's spawn path, not the
  harness, and applies to all providers uniformly.
- **In-process executor future.** Because tools live in `crates/bro-tools` and
  are provider-agnostic, a later in-process executor can reuse them without
  touching the subprocess path.
- **RTK-style per-command *output* compaction (deferred 2026-05-29).** Distinct
  from the shipped model-keyed *context-window* compaction (`compaction.rs`);
  this is per-command *output* token-saving baked into the tools at the *output*
  layer (never the command layer), eliminating the hook's command-rewrite
  mangling class by construction. Investigation found rtk's per-command filtering
  is coupled to execution (`runner::run` + large `cmds/*` modules) with no exposed
  `filter(argv, captured) -> String` dispatch, and it only handles recognized
  *single* commands. Realistic path if revisited: vendor rtk as an in-tree fork
  (`git subtree`, track upstream `develop`), mutate minimally (`lib.rs`
  re-export + telemetry-off), invoke as a subprocess for recognized single
  commands only (compound/piped → raw), always with a `raw` bypass + full-output
  tee. The historical `HOME`→`n` content mangling appears fixed in rtk 0.40.0;
  command-rewrite mangling is inherent to the *hook* and avoided by direct argv
  invocation.

## Relationship

- The transport/loop authority is [`anthropic-harness.md`](./anthropic-harness.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).

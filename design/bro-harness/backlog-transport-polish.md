---
title: "bro-harness transport & tool polish (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
brief: "Transport and model-facing tool work, including Responses retry deadlines, responsive steering, MCP resource access, and incremental tool catalogs."
---

# bro-harness transport & tool polish (backlog)

> **Provenance.** Extracted from [`anthropic-harness.md`](./anthropic-harness.md)
> "Open questions / later", enriched with the live item-5 residue from
> **thread-ca160aa2** ("bro-harness remaining work"). Items 1–4 of that thread
> (surface/client-filter split, allow/deny recursion guard, HTTP robustness with
> retry/backoff/Retry-After, resume + wire-contract test) and item 2 (SSE
> streaming on all three transports) are **done**; what follows is the residue.

## Codex transport and agent-loop adaptations

- [x] **Preserve Responses retry deadlines.** Carry server retry advice through
  rejected WebSocket upgrades, stream retries, and HTTP fallback. Keep retries
  bounded and distinguish transient failures from quota or policy failures.
  **Acceptance:** no attempt starts before the advised deadline; permanent
  failures remain terminal; fallback preserves authoritative history.
- [x] **Respond to input during inference and code-mode execution.** Add an
  explicit input notification path so inference can be preempted and running
  cells can yield without cancelling their work. Drain interrupted Responses
  streams before reusing connection and continuation state when supported.
  **Acceptance:** queued input reaches the next model step promptly, once and
  in order; cell handles and completed tool outcomes survive; explicit interrupt
  retains its cancellation semantics.
- [x] **Expose MCP resource helpers.** Provide resource listing, template
  listing, and resource reads through the session's admitted MCP connections,
  with code-mode discovery and invocation support.
  **Acceptance:** resource-only servers work; pagination, bounded output,
  configured access restrictions, and missing-server errors are explicit.
- [x] **Adopt incremental tool catalogs with Responses Lite.** Establish the
  transport and model capability boundary before emitting history-carried tool
  definitions. Preserve a stable initial catalog, append added or changed
  definitions, and communicate removals and namespace instruction changes.
  **Acceptance:** unchanged catalogs add no definitions; resume and compaction
  preserve or rebuild the correct catalog baseline; ordinary Responses requests
  retain their supported tool encoding.

Implementation owners are `transport/openai_responses*`, `agent_loop.rs`,
`code_mode.rs`, `mcp.rs`, and `registry.rs` under `crates/bro-harness/src/`.
Reference mechanisms are Codex's `responses_retry.rs`, `session/turn.rs`,
`tools/code_mode/`, `tools/spec_plan.rs`, and
`context/world_state/top_level_tools.rs` under `codex-rs/core/src/`.

## Compaction correctness

- [x] **Use one request budget.** Resolve the effective usable window from the
  model catalog or explicit policy, and use the encrypted-payload-aware history
  estimate for both proactive decisions and remote request fitting. Reserve
  inline summary output separately from remote compaction capacity.
  **Acceptance:** encrypted histories fit consistently; oversized tool output
  is trimmed on a copy; failed fitting preserves authoritative history.
- [x] **Retain the user-message boundary.** Preserve the newest user messages
  within the retained-token budget, truncating the boundary message when needed.
  **Acceptance:** an oversized latest message cannot silently erase all retained
  user text; truncation preserves valid UTF-8 and ordering.
- [x] **Honor model compatibility.** Carry catalog compaction compatibility
  hashes through session checkpoints and model changes. Compact with the previous
  model when known hashes differ, as well as when the destination budget requires
  it. **Acceptance:** equal and unknown hashes avoid unnecessary compaction;
  failed transitions preserve the previous model and checkpointed history.
- [x] **Complete the remote stream lifecycle.** Apply idle deadlines and bounded
  retries, preserve retry advice, account for compaction usage, and replace
  history only after terminal success and output validation.
  **Acceptance:** failed or incomplete streams leave history intact; cumulative
  usage includes compaction without replacing measured inference occupancy.
- [x] **Recover only replay-safe overflow rejections.** Allow compaction after a
  streamed context rejection that contains no admitted output or provider effects.
  **Acceptance:** HTTP and stream rejections recover consistently; ambiguous
  native effects and partial output remain terminal and observable.
- [x] **Select remote protocol by provider capability.** Keep ChatGPT's streamed
  trigger protocol distinct from the public standalone compact endpoint and its
  canonical output window. **Acceptance:** unsupported compatible providers use
  inline compaction; supported protocols preserve their respective output
  contracts and authentication recovery.

Reference mechanisms are `compact_remote_v2.rs`, `compact_remote_v2_attempt.rs`,
`compact_remote_history.rs`, and `session/context_window.rs` under
`codex-rs/core/src/`, plus provider capabilities and model catalog metadata.

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

> The two `web_search` bullets below are now given a fuller treatment in
> [`search-provider-abstraction.md`](./search-provider-abstraction.md) (the
> hosted-backend = its Axis B; result-normalization = its OQ-3). Retire them here
> once that doc lands.

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

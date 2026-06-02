---
title: "Claude · Robustness"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: robustness
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - robustness
brief: "Claude Code 2.1.160's Anthropic-transport robustness idioms, catalogued from the bro-harness api-robustness mine: split-system-prompt caching with a rolling message-prefix breakpoint, in-band SSE error retry that never launders an overloaded_error into a fake success turn, Retry-After-aware capped backoff (seconds + HTTP-date), role-alternation repair + tool-result padding on interrupt, server-tool block preservation + pause_turn resume, and spurious-stop detection. These are the production idioms bro-harness deliberately matched."
---

# Claude · Robustness

> **Provenance.** `design/bro-harness/bro-harness-api-robustness.md` — a graded
> review of CC 2.1.160's Anthropic-transport idioms (string literals **high**;
> decoded minified logic **medium**), much of it live-validated when bro-harness
> reimplemented it. See [snapshot](claude-2.1.160.md).

See the axis: [Robustness](../robustness.md).

## Catalogued CC idioms (high)

- **Split-system-prompt caching** — a cache-stable prefix block carries the cache
  breakpoint; a volatile tail rides uncached.
- **Rolling message-prefix cache breakpoint** — the breakpoint moves to the last
  block each turn (uses 2 of the 4 allowed breakpoints) — the canonical
  incremental-caching pattern.
- **In-band SSE error retry** — an `overloaded_error` arriving *after* the 200
  stream opened is captured, classified transient-vs-permanent, and **never
  laundered into an empty "success" turn.**
- **`Retry-After`-aware capped backoff** — handles both seconds and HTTP-date
  forms; correct retryable-status classification.
- **Role-alternation repair on interrupt** + tool-result padding so an
  interrupted dispatch never orphans a `tool_use`.
- **Server-tool block preservation + `pause_turn` resume** — `server_tool_use` /
  inline `tool_result` blocks are preserved into the replay buffer; a `pause_turn`
  (server tool hitting its iteration limit) resumes rather than terminating.
- **Spurious-stop detection** — empty-output / outstanding-async turn-end
  diagnostics.
- **Normalized usage** with a cache read/write split.

## Betas relevant to robustness (high)

`extended-cache-ttl` (1h), `token-efficient-tools`, `fine-grained-tool-streaming`,
`interleaved-thinking` — see [claude-transport](claude-transport.md) for the full
inventory and which bro-harness adopted vs deliberately opted out of.

## Open

<!-- TODO(mine): exact backoff constants/caps and jitter; the retryable-status
set; idle-timeout value; the precise pause_turn resume protocol; whether
context-overflow triggers a compact-and-retry on the CC side (bro-harness added
its own). Cross-ref bro-harness-residuals.md R1–R5 for the items left open there. -->

## Feeds

`design/bro-harness/bro-harness-api-robustness.md`,
`design/bro-harness/bro-harness-residuals.md`.

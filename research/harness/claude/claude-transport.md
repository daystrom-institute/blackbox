---
title: "Claude · Transport & Feature Flags"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: transport
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - transport
brief: "Claude Code 2.1.160 speaks the Anthropic Messages API (stateless; full history resent each turn) and opts into six betas: context-management, effort, extended-cache-ttl, fine-grained-tool-streaming, interleaved-thinking, token-efficient-tools — plus a 1M context window (claude-opus-4-8[1m]) and a user-facing fast mode. Beta inventory mined in the bro-harness api-robustness work; per-flag header values and the streaming envelope detail remain open."
---

# Claude · Transport & Feature Flags

> **Provenance.** Beta inventory and statelessness from
> `design/bro-harness/bro-harness-api-robustness.md` and
> `compaction-canonical-anthropic.md` (string-mined 2.1.160) — **high**. Model id
> / fast mode / 1M context from direct observation this session — **high**.
> Exact header values per flag are **open**. See [snapshot](claude-2.1.160.md).

See the axis: [Transport & Feature Flags](../transport.md).

## API shape (high)

- **Anthropic Messages API**, **stateless**: every turn resends the full
  `messages` array; the server holds no conversation state. Consequence —
  [compaction](claude-compaction.md) is a purely client-side buffer rewrite.

## Beta / feature flags (high inventory; open header map)

CC 2.1.160 ships **six betas** (mined): `context-management`, `effort`,
`extended-cache-ttl`, `fine-grained-tool-streaming`, `interleaved-thinking`,
`token-efficient-tools`.

User-facing knobs observed this session:

- **Fast mode** — `/fast` toggle; on Opus 4.8 it speeds output *without*
  downgrading the model (still Opus, faster decode).
- **Effort** — the `effort` beta backs an effort/reasoning-budget control.
- **1M context** — model id `claude-opus-4-8[1m]` (extended context window).
- **Extended cache TTL** — the 1h cache TTL beta (bro-harness adopted it from
  here).

## Streaming envelope (medium)

SSE stream of `content_block_start` / `*_delta` / `content_block_stop` events;
usage normalized with a **cache read/write split**. `server_tool_use` streams its
input via `input_json_delta` like a normal `tool_use`; inline `tool_result` /
`web_search_tool_result` blocks carry content in `content_block_start` (no
deltas) — confirmed by live capture in the api-robustness mine.

## Open

<!-- TODO(mine): the exact header name/value for each beta and how /fast and the
effort control map onto headers; anthropic-beta header composition; base URL /
auth-mode (OAuth vs API key) paths; whether any server-side transform is gated on
OAuth (as on the Codex/Responses side). -->

## Feeds

`design/bro-harness/anthropic-harness.md`,
`design/bro-harness/bro-harness-api-robustness.md` (§1 beta inventory; bro-harness
ships `effort`, `context-1m`, `extended-cache-ttl`).

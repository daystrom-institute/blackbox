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
brief: "Claude Code 2.1.160 speaks the Anthropic Messages API (stateless; full history resent each turn) and opts into six betas. Current CLI help adds transport/session knobs: --betas for API-key users, --fallback-model for print mode fallback, --include-partial-messages and --include-hook-events for stream-json, --json-schema structured output, --prompt-suggestions, hidden --sdk-url remote WebSocket streaming, and --bare minimal mode."
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

## CLI transport knobs (2026-06-02 local pass)

Current help exposes several machine-facing modes that were not captured in the original leaf:

- --betas adds beta headers for API-key users.
- --fallback-model switches to specified fallback model(s) for the rest of a print-mode session when the primary is overloaded or not found, while retrying the primary at the start of each user turn.
- --output-format=stream-json plus --include-partial-messages exposes partial chunks; --include-hook-events adds hook lifecycle events to the stream.
- --input-format=stream-json supports realtime streaming input; --replay-user-messages echoes stdin user messages back to stdout for acknowledgement.
- --json-schema adds structured output validation.
- --prompt-suggestions emits predicted next-prompt messages in print/SDK mode.
- --bare sets CLAUDE_CODE_SIMPLE=1 and skips hooks, LSP, plugin sync, attribution, auto-memory, background prefetches, keychain reads, and CLAUDE.md auto-discovery. It is explicitly not a no-context mode: context can still be provided through system prompt flags, --add-dir, --mcp-config, --settings, --agents, and --plugin-dir.
- A hidden --sdk-url flag in binary strings indicates remote WebSocket endpoint support for SDK I/O streaming with print + stream-json.

## Open

<!-- TODO(mine): the exact header name/value for each beta and how /fast and the effort control map onto headers; anthropic-beta header composition; base URL / auth-mode paths; hidden --sdk-url protocol; whether fallback-model is API-key-only or OAuth-compatible in every path. -->

## Feeds

`design/bro-harness/anthropic-harness.md`,
`design/bro-harness/bro-harness-api-robustness.md` (§1 beta inventory; bro-harness
ships `effort`, `context-1m`, `extended-cache-ttl`).

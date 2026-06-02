---
title: "Vibe · Robustness"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: robustness
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - robustness
brief: "Vibe robustness: three layers — Mistral SDK backoff, httpx @async_retry decorators (retryable HTTP set), and AgentLoop exception classification (rate-limit / context-too-long / non-retryable). No mid-stream reconnect; orphaned tool_calls patched with cancellation messages."
---

# Vibe · Robustness

> Mined from open source (`~/repos/mistral-vibe`, GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Robustness](../robustness.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Three retry/error layers: (1) `MistralBackend` SDK `RetryConfig` (backoff 500ms→30s, 1.5x, 5min max elapsed); (2) `GenericBackend` `@async_retry(tries=3)` on httpx, retryable set `{408,409,425,429,500,502,503,504,529}`, 0.5s base ×2; (3) `AgentLoop._chat` classifies → `RateLimitError` / `ContextTooLongError` / non-retryable (walks `__cause__` for a `non_retryable` flag) / else `RuntimeError`. **No mid-stream reconnect** — a broken stream propagates as an exception that ends the turn. Orphaned `tool_calls` are filled with cancellation messages (role-alternation repair); user-cancellation breaks the loop cleanly.

**Evidence.**
- `vibe/core/utils/retry.py:3` — retryable HTTP status set
- `vibe/core/llm/backend/mistral.py:122` — `RetryConfig(backoff…)`
- `vibe/core/agent_loop.py:1542` — `_fill_missing_tool_responses` patches orphaned tool_calls

**Vs the axis.** Confirms retry/backoff + role-alternation repair. **Divergence:** unlike Claude/codex, vibe has **no in-band mid-stream error recovery / reconnect** — it fails the turn.

## Open
<!-- Overflow path: ContextTooLongError → compaction handoff? confirm the chain. -->

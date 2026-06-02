---
title: "Codex · Robustness"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: robustness
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - robustness
brief: "Codex robustness: on stream-retry exhaustion it switches WS→HTTPS and emits a model-visible EventMsg::Warning; the fallback is monotonic (disable_websockets latch, one-way). Retry uses server-requested delay or exponential backoff, suppressing the first WS retry notice. Context-window-exceeded triggers inline-compaction history trim + retry (no sampling-layer compact). CancellationToken/TurnAborted threads through."
---

# Codex · Robustness

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Robustness](../robustness.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** `handle_retryable_response_stream_error`: on `retries >= max`, `try_switch_fallback_transport()` flips **WS→HTTPS** and (if `include_fallback_warning`) emits an `EventMsg::Warning` ("Falling back from WebSockets to HTTPS… Responses may be slower") into the turn's event stream — **model-visible**. The switch is **monotonic**: `force_http_fallback()` latches `disable_websockets=true` for the rest of the session. Below max: server-requested delay else `backoff(retries)`; first WS retry notice suppressed in release. `ContextWindowExceeded` at the sampling layer sets `total_tokens_full` and returns; the inline-compaction path instead `remove_first_item`s and retries the compaction request. `CancellationToken`/`CodexErr::TurnAborted` for interrupts.

**Evidence.**
- `core/src/responses_retry.rs:32-65` — fallback + `EventMsg::Warning`, backoff, retry-count suppression
- `core/src/responses_retry.rs` — `force_http_fallback()` latches `disable_websockets`
- `core/src/session/turn.rs` — `ContextWindowExceeded` → `set_total_tokens_full(true)`

**Vs the axis.** Confirms retry/backoff + the transport-switch-as-visible-event extension. Note: monotonic one-way WS→HTTP fallback (no reconnect) is a deliberate simplification.

## Open
<!-- Per-provider stream_max_retries values; whether HTTP can re-upgrade to WS next session only. -->

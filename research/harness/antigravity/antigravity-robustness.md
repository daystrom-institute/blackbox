---
title: "Antigravity · Robustness"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: robustness
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - robustness
brief: "SDK robustness is explicit at the client edge: previous turns are drained before a new send, concurrent receive_steps is rejected, 400/401/403 become connection errors, terminal errors become execution errors, stderr is drained, cancellation is halt_request, and disconnect escalates from close to TERM/KILL. CLI remote retry semantics remain only string/proto-inferred."
---

# Antigravity · Robustness

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Robustness](../robustness.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Retry surfaces appear as protobuf config: `ExponentialRetryConfig` (general RPC), `ModelOutputRetryConfig` (max retries; **forbid-tool-use on last retry**; force-tool-name), `ModelAPIRetryConfig`. `CancelTokenManager` for cancellation; `IsNetworkError` util. CHANGELOG: v1.0.4 fixed a "stateful callback streamer race condition during network drops"; v1.0.2 fixed a sandbox-URL-fetch nil panic. The actual retry loop is server-side; client-side resilience = gRPC reconnection + UI-hang recovery.

**Evidence.**
- `go_utils.ExponentialRetryConfig.Run`; `cortex_pb.ModelOutputRetryConfig{GetForbidToolUse,GetForceToolName}`
- CHANGELOG v1.0.4: "permanent UI hangs caused by a stateful callback streamer race condition"

**Vs the axis.** Confirms retry/backoff + the "forbid-tool-use on last retry" idiom (a spurious-tool-loop guard). **Divergence:** resilience is split client(reconnect)/server(retry).

## SDK/local harness update (2026-06-02)

The SDK gives concrete robustness behavior around the local harness boundary. Conversation.send refuses to trample an active turn; it drains outstanding messages or waits for idle before starting the next turn. LocalConnection rejects concurrent receive_steps calls, which prevents multiple consumers from racing the same step queue.

Error mapping is also explicit. System errors with codes 400, 401, or 403 raise connection-level errors. Terminal errors raise execution errors. Non-terminal system errors are logged/warned while the stream can continue. Tool errors can be intercepted by OnToolError hooks, including changing what the model sees.

Process cleanup is defensive. disconnect dispatches session-end hooks, cancels background tasks, closes WebSocket/stdin, drains stderr to avoid blocking, waits for process exit, and only then escalates to terminate/kill. cancel sends halt_request rather than killing the harness immediately.

The old binary-string retry findings still matter for CLI/cortex, but they are not enough to document exact server retry policy. Keep those as medium-confidence remote-service hints.

## Open
<!-- Server-side retry params; circuit-breaker absence; reconnect/resume-after-drop behavior. -->

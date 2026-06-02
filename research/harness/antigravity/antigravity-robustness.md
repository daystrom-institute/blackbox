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
brief: "agy robustness: protobuf retry configs (ExponentialRetryConfig, ModelOutputRetryConfig with forbid-tool-use-on-last-retry, ModelAPIRetryConfig), CancelTokenManager, IsNetworkError. The retry loop executes server-side; client resilience = gRPC reconnect + documented UI-hang/streamer-race fixes."
---

# Antigravity · Robustness

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Robustness](../robustness.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Retry surfaces appear as protobuf config: `ExponentialRetryConfig` (general RPC), `ModelOutputRetryConfig` (max retries; **forbid-tool-use on last retry**; force-tool-name), `ModelAPIRetryConfig`. `CancelTokenManager` for cancellation; `IsNetworkError` util. CHANGELOG: v1.0.4 fixed a "stateful callback streamer race condition during network drops"; v1.0.2 fixed a sandbox-URL-fetch nil panic. The actual retry loop is server-side; client-side resilience = gRPC reconnection + UI-hang recovery.

**Evidence.**
- `go_utils.ExponentialRetryConfig.Run`; `cortex_pb.ModelOutputRetryConfig{GetForbidToolUse,GetForceToolName}`
- CHANGELOG v1.0.4: "permanent UI hangs caused by a stateful callback streamer race condition"

**Vs the axis.** Confirms retry/backoff + the "forbid-tool-use on last retry" idiom (a spurious-tool-loop guard). **Divergence:** resilience is split client(reconnect)/server(retry).

## Open
<!-- Server-side retry params; circuit-breaker absence; reconnect/resume-after-drop behavior. -->

---
title: "Codex · Transport & Feature Flags"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: transport
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - transport
brief: "Codex transport: OpenAI Responses over a WebSocket-default channel (ModelClientSession) with sticky routing (x-codex-turn-state OnceLock, per-turn only), connection prewarm, and an x-codex-beta-features header (responses_websockets=2026-02-06) gating WS v2. Reasoning {effort,summary} + reasoning.encrypted_content when supported; subagent-source header per SessionSource."
---

# Codex · Transport & Feature Flags

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Transport](../transport.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** OpenAI **Responses** API over a **WebSocket-default** channel with HTTP fallback. `ModelClientSession` (per-turn) holds the WS session + an `Arc<OnceLock<String>>` **sticky-routing** turn-state (replayed in every request header *within* a turn; reusing across turns is an explicit contract violation). WS is gated by `responses_websocket_enabled()` = provider `supports_websockets` && !`disable_websockets`; a best-effort **prewarm** sends `response.create generate=false` so the first real request reuses the connection + `previous_response_id`. Header `x-codex-beta-features: responses_websockets=2026-02-06` gates WS v2. Request builder adds reasoning `{effort, summary}` (+ `include: ["reasoning.encrypted_content"]`) only when `supports_reasoning_summaries`; carries tools, `parallel_tool_calls`, verbosity, service tier. `x-codex-subagent-source` header maps SessionSource (review/compact/memory_consolidation/collab_spawn).

**Evidence.**
- `core/src/client.rs:245` — `ModelClientSession { websocket_session, turn_state: Arc<OnceLock<String>> }`
- `core/src/client.rs:798` — `responses_websocket_enabled()`; `:1694` — `build_responses_headers` (`x-codex-beta-features`, turn-state)
- `core/src/client_common.rs:20` — `Prompt { input, tools, parallel_tool_calls, base_instructions, personality, output_schema }`

**Vs the axis.** Confirms transport + betas + **WS→HTTP channel fallback**. Extends with sticky per-turn routing-state and WS prewarm — idioms beyond the Anthropic-family stateless resend.

## Open
<!-- Full beta-feature roster; service-tier/prompt-cache semantics over the wire. -->

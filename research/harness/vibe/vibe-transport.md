---
title: "Vibe · Transport & Feature Flags"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: transport
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - transport
brief: "Vibe's transport: dual backend (native Mistral SDK + a generic httpx multi-adapter supporting openai/anthropic/reasoning/openai-responses), an ACP bridge, thinking→reasoning-effort mapping, opt-in streaming. Mined from open source."
---

# Vibe · Transport & Feature Flags

> Mined from open-source Python at `~/repos/mistral-vibe` (GLM-5.1 bro, 2026-06-02). **confidence: high** (file:line + quotes). See axis: [Transport](../transport.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Two backend families selected per-provider: `MistralBackend` (native Mistral SDK) and `GenericBackend` (httpx, multi-adapter: `openai` / `anthropic` / `reasoning` / `openai-responses`, + lazy `vertex-anthropic`). Both expose `complete()` / `complete_streaming()`. Provider config is fully user-overridable (api_base, api_key_env_var, api_style). Effort is mapped: `thinking` low→`none`, medium/high/max→`high`. Streaming is opt-in at `AgentLoop` construction; programmatic mode disables it. An **ACP** (Agent Client Protocol) bridge in `vibe/acp/` wraps the loop.

**Evidence.**
- `vibe/core/llm/backend/factory.py:8` — `BACKEND_FACTORY = {MISTRAL: MistralBackend, GENERIC: GenericBackend}`
- `vibe/core/llm/backend/generic.py:30` — `_ADAPTERS = {"openai":…, "anthropic":…, "reasoning":…}`
- `vibe/core/llm/backend/mistral.py:128` — `_THINKING_TO_REASONING_EFFORT`

**Vs the axis.** Confirms multi-provider transport; extends with a native-SDK path that owns its own retry config, and an ACP surface (relevant to provider-transcript/surfaces work).

## Open
<!-- ACP wire contract detail; openai-responses adapter coverage; streaming envelope event shapes. -->

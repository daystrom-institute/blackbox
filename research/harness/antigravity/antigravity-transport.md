---
title: "Antigravity · Transport & Feature Flags"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: transport
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - transport
brief: "Standalone agy appears to be a thin client to Google's cortex/Cloud Code backend, while the SDK localharness path is WebSocket-based. Local CLI logs show fetchAvailableModels, selected-model propagation, cascade trajectory creation, and streamed conversation updates; SDK LocalConnection serializes InputEvent/OutputEvent over WebSocket to a Go harness process."
---

# Antigravity · Transport & Feature Flags

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Transport](../transport.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** gRPC bidi stream `StreamGenerateChat` to the cortex service. Model selection via `FetchAvailableModels` returning per-use-type lists (agent/command/tab/web-search/commit-message). **G1 credits** with `/credits` panel, `UseG1Credits` setting, background refresh, status-bar display. Auth = OAuth2 via system keyring, SSH-aware (prints URL on remote).

**Evidence.**
- `CloudCode_StreamGenerateChat_Handler` (gRPC bidi)
- `FetchAvailableModelsResponse.GetDefaultAgentModelId` / `.GetWebSearchModelIds`
- `ServerBackend.GetG1Credits` / `.SetUseG1Credits`

**Vs the axis.** Confirms transport + a credits/quota feature-flag surface. **Divergence:** gRPC (not REST/SSE) + a fully server-side router — the client is a rendering shell, unlike claude/codex/vibe which own the request shape.

## SDK/local harness update (2026-06-02)

The SDK transport path is inspectable. LocalConnection launches or connects to a Go local harness process and communicates over WebSocket. Python sends InputEvent messages and receives OutputEvent messages, including StepUpdate records that become Conversation steps/chunks. This path is local-process plus WebSocket, not direct model RPC from Python.

The standalone CLI path is different. Local logs show the binary creating a trajectory store manager, fetching available models from daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels, propagating a selected model label, creating a cascade trajectory, and streaming conversation updates. Binary strings expose Cloud Code and jetski/cortex proto names, but the full remote service contract is still not source-confirmed.

This gives Antigravity two transport lessons: a local harness can use a simple structured WebSocket stream for agent events, while the production CLI can keep routing, model selection, and trajectory streaming behind a hosted service.

## Open
<!-- gRPC service proto detail; how effort/reasoning is expressed over the wire. -->

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
brief: "agy transport: gRPC bidirectional streaming (StreamGenerateChat) to Google's cortex backend; per-use-type model routing via FetchAvailableModels (agent/command/tab/web-search/commit); G1 credits + /credits panel; OAuth2 via keyring with SSH detection. Thin client — routing/dispatch are server-side."
---

# Antigravity · Transport & Feature Flags

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Transport](../transport.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** gRPC bidi stream `StreamGenerateChat` to the cortex service. Model selection via `FetchAvailableModels` returning per-use-type lists (agent/command/tab/web-search/commit-message). **G1 credits** with `/credits` panel, `UseG1Credits` setting, background refresh, status-bar display. Auth = OAuth2 via system keyring, SSH-aware (prints URL on remote).

**Evidence.**
- `CloudCode_StreamGenerateChat_Handler` (gRPC bidi)
- `FetchAvailableModelsResponse.GetDefaultAgentModelId` / `.GetWebSearchModelIds`
- `ServerBackend.GetG1Credits` / `.SetUseG1Credits`

**Vs the axis.** Confirms transport + a credits/quota feature-flag surface. **Divergence:** gRPC (not REST/SSE) + a fully server-side router — the client is a rendering shell, unlike claude/codex/vibe which own the request shape.

## Open
<!-- gRPC service proto detail; how effort/reasoning is expressed over the wire. -->

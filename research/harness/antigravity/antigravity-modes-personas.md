---
title: "Antigravity · Modes, Personas & Roles"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: modes-personas
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - modes-personas
brief: "agy uses TOGGLE MODES, not personas: Planning mode (research→plan→approve→execute→verify), Fast mode (skips planning), Review mode (ArtifactReviewMode). Runtime agent states (idle/thinking/working/tool_use) are display-only. A CHAT_INTENT_FAST_APPLY intent changes behavior at dispatch level."
---

# Antigravity · Modes, Personas & Roles

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Modes, Personas & Roles](../modes-personas.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** agy has **toggle operating modes**, not personas: **Planning** ("Enabled planning mode." — the gated research→plan→approve→execute→verify workflow), **Fast** ("Enabled fast mode." — skips planning overhead), **Review** (`ArtifactReviewMode`, "Review Submission Data"). Runtime **agent states** (idle/thinking/working/tool_use/initializing) are display-only (surfaced to the statusline). `CHAT_INTENT_FAST_APPLY` is a distinct chat-intent enum — behavior changes at the intent/dispatch level, not just UI.

**Evidence.**
- "Enabled planning mode." / "Enabled fast mode."; `ArtifactReviewMode`
- `CHAT_INTENT_FAST_APPLY` chat-intent enum
- statusline agent_state values

**Vs the axis.** Confirms the operating-mode facet (plan/fast/review). **Divergence:** **no persona/communication-style layer** and roles are server-side — agy's modes are behavior toggles + chat-intents, the leanest of the four on this axis.

## Open
<!-- Whether modes swap the system prompt (server-side) or just gate the workflow. -->

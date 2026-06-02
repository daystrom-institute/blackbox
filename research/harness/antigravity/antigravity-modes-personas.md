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
brief: "SDK persona control is explicit through system_instructions: plain append, TemplatedSystemInstructions identity/sections, or CustomSystemInstructions replacement, plus ThinkingLevel and model selection. Standalone agy still exposes planning/fast/review mode signals and runtime status states."
---

# Antigravity · Modes, Personas & Roles

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Modes, Personas & Roles](../modes-personas.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** agy has **toggle operating modes**, not personas: **Planning** ("Enabled planning mode." — the gated research→plan→approve→execute→verify workflow), **Fast** ("Enabled fast mode." — skips planning overhead), **Review** (`ArtifactReviewMode`, "Review Submission Data"). Runtime **agent states** (idle/thinking/working/tool_use/initializing) are display-only (surfaced to the statusline). `CHAT_INTENT_FAST_APPLY` is a distinct chat-intent enum — behavior changes at the intent/dispatch level, not just UI.

**Evidence.**
- "Enabled planning mode." / "Enabled fast mode."; `ArtifactReviewMode`
- `CHAT_INTENT_FAST_APPLY` chat-intent enum
- statusline agent_state values

**Vs the axis.** Confirms the operating-mode facet (plan/fast/review). **Divergence:** **no persona/communication-style layer** and roles are server-side — agy's modes are behavior toggles + chat-intents, the leanest of the four on this axis.

## SDK/local harness update (2026-06-02)

The SDK reframes the persona part of this axis. persona_config examples show three instruction modes: a simple string appended to the default prompt, TemplatedSystemInstructions with identity and section fields, and CustomSystemInstructions that replaces the default prompt. This is not just a UI mode toggle; it is a programmatic contract for default-prompt extension versus replacement.

AgentConfig also exposes model and thinking_level. types.py defaults the main model to gemini-3.5-flash and image generation to gemini-3.1-flash-image-preview. Local CLI logs showed selected model override propagation using a UI label, which suggests model routing is partly server/catalog-driven in standalone agy.

The earlier CLI mode findings still stand as CLI-specific behavior: planning/fast/review modes and statusline agent states appear in strings/changelog. Treat SDK personas and CLI modes as overlapping but distinct axes.

## Open
<!-- Whether modes swap the system prompt (server-side) or just gate the workflow. -->

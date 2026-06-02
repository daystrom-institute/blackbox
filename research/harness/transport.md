---
title: "Axis: Transport & Feature Flags"
kind: research-axis
corpus: blackbox-research
track: harness
axis: transport
topic:
  - harness
  - transport
brief: "Cross-harness invariant model for the transport axis: the API shape a harness speaks (Anthropic Messages, OpenAI Responses, Chat Completions, Google GenAI, custom), the control headers and beta/feature gates it sets (effort, fast mode, reasoning/thinking budgets, cache TTL, token-efficient tools, interleaved thinking), the streaming envelope, and transport-channel fallback (WS → SSE → HTTP). Synthesis of the per-subject transport cells."
---

# Axis: Transport & Feature Flags

> **Scope.** The wire and the knobs on it. *What protocol* the harness speaks to
> the model provider, *which optional capabilities* it opts into via headers/beta
> flags, and *how the bytes stream*. Not the agent loop that consumes the
> transport (see [agent-loop](agent-loop.md)) — only the channel and its
> negotiated features.

## The dimension

A harness's transport is the foundation everything else rides. It determines
statefulness (does the server hold conversation state, or does every turn resend
the full history?), what features are reachable, and how robust the byte channel
is. Feature flags are folded in here because they are *transport-level
negotiation* — headers and beta opt-ins set at connection/request time.

## Questions a finding must answer

- **API shape.** Which contract? (Anthropic Messages / OpenAI Responses /
  Chat Completions / Google GenAI / custom.) Stateful or stateless?
- **Endpoints & auth.** OAuth vs API key; which base URL; any auth-mode-specific
  paths (e.g. server-side transforms only on the OAuth path).
- **Control headers / beta gates.** What betas does it set, and when? Catalogue:
  effort, fast mode, reasoning/thinking budget, cache TTL (1h?), token-efficient
  tools, interleaved thinking, context-management, fine-grained tool streaming.
- **Streaming envelope.** SSE event types; how content blocks / deltas arrive;
  how usage is reported (cache read/write split?).
- **Channel fallback.** Does it prefer WebSocket and fall back to SSE/HTTP? How
  is stickiness/reconnect handled?
- **Feature flag → behavior map.** Which user-facing knob (e.g. `/fast`, effort
  level) sets which header/value?

## Convergence / divergence

| Subject | API shape | Stateful? | Key betas/flags | Channel | Cell |
|---|---|---|---|---|---|
| Claude | Anthropic Messages | stateless | _TBD_ | _TBD_ | [claude](claude/claude-transport.md) |
| Codex | OpenAI Responses | _TBD_ | _TBD_ | WS→HTTP | [codex](codex/codex-transport.md) |
| Antigravity | Google / Antigravity | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-transport.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-transport.md) |

## Open invariants

<!-- TODO(synthesis): fill as cells land. Candidate invariants to confirm: -->
- Is "resend full history each turn" (stateless) the Anthropic-family norm vs a
  server-side state token on the Responses/OAuth path?
- Do all subjects converge on SSE as the lowest-common-denominator channel?
- Is there a portable notion of "effort" across providers, or is each bespoke?

## Codex-lens extensions

- **Channel fallback is confirmed cross-harness** (Claude WS-aware; Codex
  WS→HTTPS). Extension: the fallback may be surfaced to the model as a **visible
  warning event** (with noise-suppression on the first retry) — the model must
  tolerate a mid-stream transport change, not just the harness.

## Feeds

`design/bro-harness/anthropic-harness.md`,
`design/bro-harness/brodex-websocket-transport.md`,
`design/bro-harness/bro-harness-api-robustness.md` (the §1 beta inventory).

---
title: "Vibe · Compaction"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: compaction
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - compaction
brief: "Vibe compaction: client-side, AutoCompactMiddleware @ ~200k tokens, a 'CONTEXT CHECKPOINT COMPACTION' handoff summarizer prompt via a separately-configurable compaction_model, prior-user-message preservation (20k budget, middle-truncation), rebuild as [system, prior_user_msgs, summary] + session reset."
---

# Vibe · Compaction

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Compaction](../compaction.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Client-side. `AutoCompactMiddleware.before_turn` fires `COMPACT` when `context_tokens >= auto_compact_threshold` (default 200k, per-model configurable). `AgentLoop.compact()` sends full history to the LLM with a configurable "compact" prompt (a **handoff** summarizer — framed as a checkpoint between LLM instances), using a separate `compaction_model` (defaults to active). Prior non-injected user messages are preserved (newest-first, 20k budget, middle-truncation). Buffer rebuilt as `[system, *prior_user_messages, summary]`; session id reset; `context_tokens` zeroed. A `ContextWarningMiddleware` (opt-in, 50%) injects a usage warning earlier.

**Evidence.**
- `vibe/core/middleware.py:117` — `AutoCompactMiddleware.before_turn` threshold check
- `vibe/core/agent_loop.py:1681` — `compact()` full flow
- `vibe/core/prompts/compact.md` — `"You are performing a CONTEXT CHECKPOINT COMPACTION…"`

**Vs the axis.** Confirms client-side rebuild `[summary, …tail]`. **Idiom:** the explicit "handoff between LLM instances" framing + verbatim prior-user-message retention (vs Claude's split-point tail).

## Open
<!-- Exact threshold defaults per model; whether compaction_model is ever a cheaper model by default. -->

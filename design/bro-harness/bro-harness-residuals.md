---
title: "bro-harness Anthropic transport / agent loop — residuals"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - api
brief: "The deliberate residuals left after the Anthropic-transport correctness/completeness pass (companion to bro-harness-api-robustness.md, where the landed work is recorded). Each item here is left open on purpose — optional, gated, near-unreachable, or requiring validation not available in the pass — with the rationale and the trigger that would make it worth doing. None is a correctness gap in landed behavior. Cross-references the brodex compaction follow-ons (the OpenAI/shared lane) so the two residual sets compose."
---

# bro-harness residuals (Anthropic transport + agent loop)

> **Context.** The correctness/robustness pass landed extended cache TTL, backoff
> jitter, SSE idle timeout + retryable mid-stream read, server-tool block
> preservation + `pause_turn` resume, and the structured compaction prompt — all
> live-validated (see `bro-harness-api-robustness.md` §6). This doc tracks what
> was *deliberately* left open and why, so a future agent doesn't re-derive the
> reasoning or mistake a residual for an oversight. Companion residual set for the
> OpenAI/shared compaction lane: `brodex-compaction-followons.md` (1b token
> budgets, phase-4 hooks/analytics) — owned by that lane, not duplicated here.

## R1 — Hard blocking floor + `count_tokens` pre-probe `[cross-transport]`

**State:** optional. The brodex phase-3 *proactive trigger* already compacts on a
projected size (`last_prompt_tokens + pending_input_estimate`) before an
overflowing request is sent, and `bound_tool_result` caps oversized results, so
the realistic over-window window is small.

**What's left:** a *hard floor* that refuses to compose a request which would
still overflow even after compaction, and an optional `/v1/messages/count_tokens`
pre-probe to catch a single step that jumps from under-threshold to over-window.

**Trigger to do it:** observed over-window 400s in practice. Until then the
estimate-based trigger is sufficient and a count_tokens round-trip per turn is not
worth the latency. Reference: `compaction-canonical-anthropic.md` §4 (CC's
fixed-headroom + blocking-floor model).

## R2 — Volatile system-tail placement on Anthropic `[anthropic]`

**State:** low-severity inconsistency, left as-is. On Anthropic the volatile
system block sits in the `system` array *before* the messages (`anthropic.rs`
`build_body`); the OpenAI transports place the volatile tail *trailing* (after the
conversation). When the volatile block actually changes (a nudge fires,
`tail_nudge` set) it sits mid-prefix and invalidates the rolling message-prefix
cache for that turn.

**Why not fixed now:** in steady state the volatile text is byte-identical
turn-to-turn, so the prefix still matches and caching is unaffected — the cost is
only on the occasional nudge turn. More importantly, the split-system shape
(cached stable prefix + uncached volatile suffix) *is* the idiomatic Anthropic
caching pattern; moving volatile "after messages" on Anthropic is awkward (no
`developer` role; appending to the last user message would leak volatile content
into the persisted replay buffer) and shifts where the rolling breakpoint must
sit. The risk outweighs the marginal cache gain on nudge turns.

**Trigger to do it:** measured cache-hit-rate loss attributable to nudge turns on
a long session. The fix would append volatile as a transient trailing block that
is stripped from the persisted buffer, matching the other transports.

## R3 — Tool-def cache breakpoint fallback `[anthropic]` (skip)

**State:** deliberately skipped. The cache breakpoint rides `system[0]`; in the
canonical order `tools → system → messages`, a breakpoint on the stable system
block already caches the (large) tools array. The only gap is when there is **no
system prompt at all**, which does not occur in practice — `compose_system`
always emits the daemon system text. Adding a tool-def breakpoint would spend code
and one of the 4 breakpoints on an unreachable edge.

**Trigger to do it:** a real code path that runs `run_turn` with an empty system
prompt and a large tools array.

## R4 — `interleaved-thinking` replay `[anthropic]` (gated feature)

**State:** intentional opt-out, not a residual bug. Thinking is captured
display-only and never replayed (no signature persisted), so on reasoning-heavy
models the cross-turn reasoning chain is not carried forward. Adopting it means
persisting `thinking` blocks *with* their `signature` and adding the
`interleaved-thinking-2025-05-14` beta — a transport + buffer change with its own
correctness surface (signature handling, buffer growth).

**Trigger to do it:** a use case that needs multi-step reasoning continuity across
turns on a reasoning model. See `bro-harness-api-robustness.md` §4.1.

## R5 — Live `pause_turn` resume-loop repro `[anthropic]` (validation)

**State:** the resume *mechanism* is implemented and the load-bearing property —
the provider accepting a replayed buffer that contains `server_tool_use` +
`tool_result` blocks — is live-validated against GLM (HTTP 200, model answered
from the preserved result). What was **not** force-triggered is an actual
`pause_turn` (GLM completed within `max_uses:5`, below the 10-iteration server
pause). So the resume loop itself rests on the documented Anthropic protocol plus
the validated replay-acceptance, not a live pause repro.

**Trigger to do it:** drive a real `pause_turn` — a provider that pauses (real
Claude, or GLM with `max_uses` raised and a query that needs many searches) — and
confirm the merged single-assistant buffer and the `MAX_PAUSE_RESUMES` bound. Low
risk given the replay acceptance is already proven; this is belt-and-suspenders.

## Pointers

- Landed work + the API review: `bro-harness-api-robustness.md`.
- Canonical Anthropic compaction model: `compaction-canonical-anthropic.md`.
- OpenAI/shared compaction follow-ons (other lane): `brodex-compaction-followons.md`.
- Code: `crates/bro-harness/src/transport/anthropic.rs`,
  `crates/bro-harness/src/transport/mod.rs`,
  `crates/bro-harness/src/compaction.rs`,
  `crates/bro-harness/src/agent_loop.rs`.

---
title: "bro-harness API interaction review & robustness roadmap"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - api
brief: "A graded review of bro-harness's Anthropic-transport API interactions against Claude Code's idioms (mined from the 2.1.160 binary), plus a prioritized roadmap to make the harness robust and SOTA. Captures one real latent bug (pause_turn), the deliberate beta opt-outs and when to revisit them, predictive-vs-reactive compaction, and two caching edge cases. The two cheap wins (extended cache TTL, backoff jitter) are already landed and recorded here as a complete ledger."
---

> **Scope.** The deep read was the **Anthropic transport** (`transport/anthropic.rs`,
> `transport/mod.rs`, `transport/http.rs`, `agent_loop.rs`). Anthropic-specific
> items are marked `[anthropic]`; items that also touch the OpenAI transports are
> marked `[cross-transport]`. The reference baseline is **Claude Code 2.1.160**, a
> mature Anthropic-transport consumer (reverse-derived from the shipped binary;
> string literals high-confidence, decoded minified logic best-effort). Companion:
> `compaction-canonical-anthropic.md` (the summarizer-prompt + trigger model).

## 1. Verdict & baseline

The bro-harness Anthropic API layer is already **modern and idiomatic** — not a
naive client. What it does well, and should be preserved through any refactor:

- **Split-system prompt caching** — a cache-stable prefix block carries the
  breakpoint; a volatile tail rides uncached (`anthropic.rs:82-96`).
- **Rolling message-prefix cache breakpoint** on the last block each turn
  (`messages_with_cache_breakpoint`, `anthropic.rs:600-612`) — 2 of the 4
  allowed breakpoints, the canonical incremental-caching pattern.
- **In-band SSE error retry** — an `overloaded_error` that arrives *after* the
  200 stream opened is captured, classified transient-vs-permanent, and never
  laundered into an empty "success" turn (`anthropic.rs:195-208, 414-450`).
- **`Retry-After`-aware capped backoff** for both seconds and HTTP-date forms,
  correct retryable-status classification (`http.rs`).
- **Role-alternation repair on interrupt** (`note_interrupted`,
  `anthropic.rs:524-536`) and tool-result padding so an interrupted dispatch
  never orphans a `tool_use` (`agent_loop.rs:656-676`).
- **Spurious-stop detection** — empty-output / outstanding-async turn-end
  diagnostics (`agent_loop.rs:733-797`).
- **Normalized usage with cache read/write split** (`mod.rs:40-65`).

CC ships six betas: `context-management`, `effort`, `extended-cache-ttl`,
`fine-grained-tool-streaming`, `interleaved-thinking`, `token-efficient-tools`.
After the §6 landings, bro-harness ships `effort`, `context-1m`,
`extended-cache-ttl`. The remaining three are addressed below — two as
deliberate opt-outs, one as not-applicable.

## 2. 🔴 Real latent bug — `pause_turn` not handled `[anthropic]`

**Symptom.** `map_stop` (`anthropic.rs:182-188`) maps any unknown stop reason,
including `pause_turn`, to `StopReason::Other`. The loop treats any non-`ToolCalls`
stop as terminal (`agent_loop.rs:609`), so the turn ends as `"model_stop"`.

**Why it matters.** bro-harness enables the server-side `web_search_20250305`
tool (`anthropic.rs:69-74`). CC's own docs (verbatim in the binary): *"the
response will have `stop_reason: "pause_turn"`. To continue, re-send the user
message and assistant response."* A long server-tool turn that pauses would
**silently truncate** instead of resuming. Low frequency (only server tools, only
long operations), but a correctness hole whenever `web_search` is on.

**Why it's not a one-liner.** `run_turn` already appended the partial assistant
message to the buffer (`anthropic.rs:496-497`) before returning. Naively looping
again would push a *second* assistant message and 400 on strict alternation. A
correct fix needs the transport to recognize "the last buffered message is a
paused assistant turn" and **continue** it on the next request rather than append
a fresh one.

**Deeper prerequisite — server-tool block preservation.** CC's binary clarifies
the trigger: pause_turn fires when a *server* tool "reaches its default limit of
10 iterations … re-send the user message and assistant response … the server will
resume where it left off." Resuming therefore means re-sending the paused
assistant's **full content blocks**, including the `server_tool_use` and
`web_search_tool_result` blocks. But `fold_sse` only reconstructs `text` /
`thinking` / `tool_use` and **discards** everything else (`anthropic.rs` block
reconstruction, the `_ => {}` arm), so those server blocks never enter the replay
buffer. A correct resume thus depends on first **capturing and replaying the
server-tool blocks** — a larger change than the loop wiring alone. Frequency is
further bounded by `max_uses:5` on the harness's `web_search` tool
(`anthropic.rs:69-74`), which usually completes within a single response before
the 10-iteration pause. Net: real, but low-urgency and a two-part change (server
block capture, then resume loop).

**Proposed design.**
- Add `StopReason::Paused` to the normalized enum (`mod.rs:86-95`); map
  `"pause_turn"` to it in `anthropic.rs` (and, defensively, in the OpenAI
  transports' stop mapping, which have their own long-tool semantics).
- In the loop, treat `Paused` like `ToolCalls` for *continuation* (don't break)
  but with **no tool dispatch** — just re-enter `run_turn`.
- Give the transport a `resume_paused: bool` turn flag (or detect "last message
  is assistant" inside `run_turn`) so the paused assistant content is re-sent and
  extended in place, preserving alternation. The Messages API resumes by
  re-sending the partial assistant turn as the trailing message.
- Bound resumes (e.g. ≤ 3 consecutive `pause_turn`s) to avoid a pathological
  pause loop; surface a turn-end diagnostic if exceeded.
- Test: a synthetic SSE sequence ending in `message_delta.stop_reason:"pause_turn"`
  drives one continuation, and the rebuilt buffer stays alternation-valid.

## 3. 🟡 Predictive vs reactive compaction `[cross-transport]`

**Today.** The compaction trigger reads the *previous* turn's usage
(`agent_loop.rs:575` sets `last_prompt_tokens`; the check at `526` runs at the top
of the next turn). So compaction lags by one turn, and the over-threshold request
has already been sent. A single large step can cross from under-threshold to
over-window before compaction fires.

**Mitigation already present.** `bound_tool_result` (`agent_loop.rs:632-638`)
spills oversized tool results to disk and inlines a head+rider, so the most common
blowup vector (a giant file read) is capped before it enters the buffer. This
makes the reactive lag tolerable in practice.

**CC's approach.** Pre-counts with `/v1/messages/count_tokens` and keeps a
*blocking floor* — it refuses to compose a turn that would overflow even
post-compaction (see `compaction-canonical-anthropic.md` §4).

**Proposed (small, ordered).**
1. **Blocking floor** — before `run_turn`, if `last_prompt_tokens` already exceeds
   a hard `window − reserved` floor, compact *first* (already the path) and, if
   still over, refuse with a clear error rather than send an over-window request.
   This is the highest-value robustness add and is shared with the compaction doc.
2. **(Optional) Pre-count** — a `count_tokens` probe before composing when the
   buffer grew sharply, to catch a single-step blowup the `bound` cap didn't.
   Worth it only if blocking-floor refusals are observed in practice.

## 4. 🟢 Deliberate beta opt-outs — keep, but document the trigger to revisit

These are correct *today*; the doc records the cost and the condition under which
adopting them becomes worthwhile, so a future agent doesn't re-litigate blindly.

### 4.1 `interleaved-thinking-2025-05-14` `[anthropic]`
Thinking is captured display-only and **not** replayed into the buffer — no
signature is persisted (`anthropic.rs:463-469`, `mod.rs:100-107`). Correct,
because replaying a thinking block requires its matching `signature`, which the
harness doesn't store. **Cost:** on reasoning-heavy models the cross-turn
reasoning chain is lost; each turn re-reasons from scratch. **Revisit when:** we
want multi-step reasoning continuity (e.g. long agentic planning on a reasoning
model) — then persist `thinking` blocks *with* their `signature` and add the beta.
This is a transport + buffer change, not a flag flip.

### 4.2 `context-management-2025-06-27` + `memory_20250818` `[anthropic]`
CC offloads tool-result clearing (and a memory tool) to the server. bro-harness
does **manual compaction** instead (`Transport::compact`), which is uniform across
all three transports and keeps full control of what survives a cut. **Keep.**
**Revisit when:** Anthropic's server-side context editing materially beats our
summarizer on cost/quality *and* we're willing to fork behavior per-transport
(the OpenAI side has no equivalent). Until then, uniform manual compaction is the
right call.

### 4.3 Not applicable / correctly unused
- **`fine-grained-tool-streaming-2025-05-14`** — we buffer `input_json_delta`
  then parse at block close (`anthropic.rs:471-476`), relying on the API's
  valid-JSON-at-close guarantee. Fine-grained streaming forfeits that guarantee
  for earlier partial visibility we don't need. **Correctly unused.**
- **`token-efficient-tools-2025-02-19`** — a Claude-3.7-era feature; not relevant
  to the current model targets. **Skip.**

## 5. 🔵 Caching edge cases `[anthropic]`

Neither is a steady-state defect; both are cheap robustness polish.

### 5.1 Tools uncached when the system prompt is empty
The cache breakpoint rides `system[0]` (`anthropic.rs:86-89`). With no system
prompt (`empty_system_is_omitted`), the canonical order `tools → system →
messages` leaves the (often large) `tools` array with **no breakpoint before it**,
so tools aren't cached. Edge case — the harness almost always has a system prompt
— but a breakpoint on the **last tool definition** closes it and is order-robust.

### 5.2 Volatile system-tail placement is inconsistent across transports
On Anthropic the volatile block sits **in the system array, before the messages**
(`anthropic.rs:91-93`). The other transports place the volatile tail **trailing**,
after the conversation (OpenAI Chat: trailing system message; Responses: trailing
`developer` item — `mod.rs:120-130`). Because the Anthropic volatile block is
mid-prefix, the turn where it *actually changes* (a nudge fires, `tail_nudge` is
set — `agent_loop.rs:553-559`) invalidates the rolling **message-prefix** cache
for that turn, not just itself. **Steady state is fine** (volatile is usually
byte-identical, so the prefix still matches), so this is low-severity — but it's a
real inconsistency. **Option:** move the Anthropic volatile tail to a trailing
position (e.g. appended to the last user/tool_result message or a trailing system
turn) to match the other transports and protect the message cache on nudge turns.
Needs care: a trailing block changes where the rolling breakpoint should sit.

## 6. ✅ Landed in this pass (cheap wins) `[anthropic / cross-transport]`

Recorded here so the review is a complete ledger.

- **Extended cache TTL `[anthropic]`** — added `extended-cache-ttl-2025-04-11` to
  `DEFAULT_ANTHROPIC_BETAS` and a `cache_control()` helper defaulting to
  `ttl:"1h"` on both breakpoints (`anthropic.rs`). Agent turns routinely have
  multi-minute gaps (tool runs, human steering) that expire the default 5-minute
  ephemeral cache and re-pay full prefix processing; 1h keeps it warm. Tunable via
  `BRO_HARNESS_CACHE_TTL` (empty → plain ephemeral, no beta) as a per-provider
  escape hatch. **Verified live 2026-06-02** on GLM (z.ai) and DeepSeek: both
  accept the `extended-cache-ttl` beta + `ttl:"1h"` shape (HTTP 200) and engage
  prompt caching — a warm request returned `cache_read_input_tokens=1792` on both
  (input dropped from ~1.8k to ~20). Real-Claude is known-good. No fallback
  needed; the empty-TTL knob remains for any future provider that rejects it.
- **Backoff jitter `[cross-transport]`** — `backoff()` now applies ±20%
  dependency-free jitter over a deterministic `backoff_base()`, re-capped at the
  ceiling (`http.rs`), so a fleet tripping a shared 429 doesn't retry in lockstep.
- **SSE idle timeout on the Anthropic transport `[anthropic]`** — `run_turn`'s
  consume loop now wraps `stream.next()` in `tokio::time::timeout(idle, …)`
  (`stream_idle_timeout()`, default 300s), which the OpenAI Responses transports
  already had but the Anthropic one lacked — so a half-open stream (connection up,
  no events) failed over only at the 600s request timeout. Same pass made a
  **mid-stream read error retryable** (it previously hard-failed via `?`), and
  both new retry paths are gated on a `streamed_content` flag so a retry can never
  re-stream already-emitted content (the same dedup-safe guard the Responses
  transport uses). This guard now also covers the pre-existing overloaded-error
  retry, closing a latent duplicate-text window when an overload arrived
  mid-content.

## 7. Priority roadmap

| # | Item | Severity | Effort | Status |
| --- | --- | --- | --- | --- |
| 1 | `pause_turn` continuation + server-block capture (§2) | 🔴 correctness | L (2-part) | proposed |
| 2 | Blocking floor before compose (§3.1) | 🟡 robustness | S | proposed |
| 3 | Volatile tail → trailing on Anthropic (§5.2) | 🔵 polish | S–M | proposed |
| 4 | Tool-def cache breakpoint fallback (§5.1) | 🔵 polish | S | proposed |
| 5 | Structured compaction prompt (compaction doc) | 🟡 quality | S | proposed |
| 6 | `count_tokens` pre-probe (§3.2) | 🔵 optional | S | deferred |
| 7 | interleaved-thinking replay (§4.1) | 🟢 feature | L | gated |
| — | Extended cache TTL (§6) | 🟡 cost | S | **landed** |
| — | Backoff jitter (§6) | 🔵 robustness | S | **landed** |
| — | Anthropic SSE idle timeout + retryable read error (§6) | 🟡 robustness | S | **landed** |

Suggested next slice: **#1 (pause_turn)** as its own focused change, then **#2
(blocking floor)** folded with the structured-prompt work (#5) since both live in
the compaction seam.

## 8. Validation notes

- Transport-shape changes: unit-test the wire body (the existing `build_body` /
  `messages_with_cache_breakpoint` tests are the model) plus an SSE-sequence test
  for any new stop-reason path (`fold_sse` tests are the model).
- `pause_turn`: assert the rebuilt buffer stays alternation-valid after a
  continuation; assert the resume bound trips.
- Provider behavior (extended TTL on GLM/DeepSeek, pause_turn on real web_search):
  per project convention, validate against the live provider with narrow,
  authorized probes — not unit tests alone. Build worktrees against a shared
  `CARGO_TARGET_DIR`.
- Never assert against the prod daemon or real `$HOME`; isolated state only.

## 9. References

- `crates/bro-harness/src/transport/anthropic.rs` — body build, SSE fold, cache
  breakpoints, stop mapping, compact.
- `crates/bro-harness/src/transport/mod.rs` — `Transport` trait, `Usage`,
  `StopReason`, `SystemPrompt`, `TurnOpts`.
- `crates/bro-harness/src/transport/http.rs` — retry, backoff (+ jitter),
  `Retry-After`.
- `crates/bro-harness/src/agent_loop.rs` — the turn loop, compaction trigger,
  interrupt handling, turn-end diagnostics.
- `design/bro-harness/compaction-canonical-anthropic.md` — summarizer prompt +
  trigger model (companion).
- `design/bro-harness/anthropic-harness.md` — transport/agent-loop as-built.
- Mined: Claude Code `2.1.160` binary — betas, `pause_turn` resume protocol,
  cache TTL, `count_tokens` discipline.

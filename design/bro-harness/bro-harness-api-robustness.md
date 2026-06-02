---
title: "bro-harness API interaction review & robustness roadmap"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - api
brief: "A graded review of bro-harness's Anthropic-transport API interactions against Claude Code's idioms (mined from the 2.1.160 binary), with the correctness/robustness work now landed and live-validated: extended 1h cache TTL, backoff jitter, SSE idle timeout + retryable mid-stream read, server-tool (web_search) block preservation + pause_turn resume, and a structured compaction summary prompt with <summary> extraction. The predictive compaction trigger landed separately as brodex phase 3. Remaining items (R1–R5: optional blocking floor, volatile-tail placement, tool-def breakpoint, interleaved-thinking, live pause repro) are deliberate residuals consolidated in bro-harness-residuals.md. §4 records the deliberate beta opt-outs and when to revisit them."
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

## 2. ✅ LANDED — server-tool block preservation + `pause_turn` resume `[anthropic]`

**The original bug.** The Anthropic block reconstruction handled only
`text`/`thinking`/`tool_use` and dropped everything else via the `_ => {}` arm.
So a server-side web_search turn lost its `server_tool_use` and result blocks
from the replay buffer entirely — the model could not see its own search results
on any later turn — and a `pause_turn` (server tool hitting its iteration limit)
was mapped to a generic stop and silently terminated the turn instead of resuming.

**Ground truth (live capture).** A GLM web_search turn streams
`[text, server_tool_use, text, tool_result, text]`: `server_tool_use` streams its
input via `input_json_delta` (like `tool_use`); the `tool_result` carries its
content inline in `content_block_start` (no deltas). This removed the earlier
speculation — the wire shape is confirmed, not assumed.

**Implemented (`anthropic.rs`).**
- `SseBlock` gains a `raw` field; `fold_sse` captures `server_tool_use` id/name
  (+ streamed input) and the inline result block (`tool_result` /
  `web_search_tool_result`) verbatim. `reconstruct_segment` emits them back into
  the assistant `content` for faithful replay, but does **not** surface them as
  client `tool_calls` (the server already ran them).
- `run_turn` is now a single loop that handles in-band retry **and** `pause_turn`
  resume: on a pause it appends the partial assistant, rebuilds the body (which
  re-sends it so the server continues), and merges every segment into ONE
  assistant message so the buffer stays alternation-valid. Bounded by
  `MAX_PAUSE_RESUMES`; the in-band retry budget resets per segment. No
  `StopReason` or agent-loop change — fully encapsulated in the transport.

**Validation.** Unit tests cover the SSE capture and reconstruction. Validated
end-to-end against GLM: replaying `[user, assistant(with server blocks), user]`
returns **HTTP 200** and the model answers from the preserved `tool_result`
content (it restated the version it had searched) — proving the provider accepts
the replayed server blocks, which is the same acceptance the resume path relies
on. *Residual:* a real `pause_turn` was not force-triggered live (GLM completed
within `max_uses`), so the resume *loop* is covered by the documented protocol +
the validated replay-acceptance rather than a live pause repro. Tracked in the
residuals doc.

## 3. 🟢 Predictive compaction — addressed on main (brodex phase 3) `[cross-transport]`

The reactive-lag concern this section originally raised is **largely addressed by
landed work**: the brodex compaction phase-3 *proactive trigger* now compacts on
a *projected* size — `projected_tokens = last_prompt_tokens + pending_input_estimate`
(`agent_loop.rs`) — so an appended tool result / user message that would overflow
the *next* request triggers compaction **before** it is sent, not a turn late.
Combined with `bound_tool_result` (oversized results spill to disk before
entering the buffer), the single-step-blowup window is small.

**Remaining (optional, low-priority residual).** A hard *blocking floor* — refuse
to compose a request that would still overflow even after compaction — and a
`count_tokens` pre-probe would close the last gap (a single step that jumps from
under-threshold to over-window). Worth it only if over-window 400s are actually
observed; the proactive estimate covers the common case. See
`compaction-canonical-anthropic.md` §4 and the residuals doc.

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
- **Server-tool block preservation + `pause_turn` resume `[anthropic]`** (§2) —
  web_search `server_tool_use`/`tool_result` blocks are now captured and replayed,
  and `pause_turn` resumes the server-tool turn. Live-validated against GLM.
- **Structured compaction summary prompt + `<summary>` extraction
  `[anthropic / cross-transport]`** — `COMPACTION_INSTRUCTION` is now the canonical
  9-section structured prompt with an `<analysis>` scratchpad and mandatory
  verbatim security-constraint preservation; a shared `transport::extract_summary`
  keeps only the durable `<summary>` block across all three inline transports.
  Live-validated on GLM + DeepSeek: both returned all 9 sections, a clean
  extractable summary, and preserved the security constraint verbatim. The
  per-tool-result render cap and 8192-token summary budget were already lifted by
  brodex compaction phase 1 (`CompactionParams`).

## 7. Priority roadmap

| # | Item | Severity | Effort | Status |
| --- | --- | --- | --- | --- |
| — | Extended cache TTL | 🟡 cost | S | **landed** |
| — | Backoff jitter | 🔵 robustness | S | **landed** |
| — | Anthropic SSE idle timeout + retryable read error | 🟡 robustness | S | **landed** |
| — | Server-tool block preservation + `pause_turn` resume (§2) | 🔴 correctness | L | **landed** |
| — | Structured compaction prompt + `<summary>` extraction | 🟡 quality | M | **landed** |
| — | Predictive compaction trigger (§3) | 🟡 robustness | — | **landed (brodex ph3)** |
| R1 | Hard blocking floor + `count_tokens` pre-probe (§3) | 🔵 optional | S | residual |
| R2 | Volatile tail → trailing on Anthropic (§5.2) | 🔵 polish | S–M | residual |
| R3 | Tool-def cache breakpoint fallback (§5.1) | 🔵 polish | S | residual (skip) |
| R4 | interleaved-thinking replay (§4.1) | 🟢 feature | L | gated |
| R5 | Live `pause_turn` resume-loop repro | 🔵 validation | S | residual |

Open residuals (R1–R5) are consolidated with rationale in
**`bro-harness-residuals.md`**. Everything correctness-critical for the Anthropic
transport and agent loop is landed and live-validated.

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

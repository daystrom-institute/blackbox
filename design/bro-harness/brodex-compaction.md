---
title: "Canonical compaction for brodex (OAI Responses), per codex reference"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - brodex
  - compaction
brief: "Ground-truth spec for how context compaction SHOULD work on the brodex (OpenAI Responses) transport, taken from openai/codex main. Codex treats compaction as a server-side transform that returns an encrypted Compaction item (store:false-friendly), gated by provider support, with a token-budgeted verbatim retention tail, fit-trimming so the compaction request itself fits the window, proactive pre-sampling triggering, and a local inline summarizer fallback for providers that don't support remote compaction. Compares against bro-harness's current client-side plaintext-summarize implementation and lays out a phased path to make brodex rock-solid and OAI-idiomatic."
---

# Canonical compaction for brodex (OAI Responses), per codex reference

> **Status: the canonical set (phases 0–3) is landed.** Landed: the
> context-window overflow recovery (`77d0514`); the canonical **server-side
> `responses/compact`** for the ChatGPT-OAuth path (live-validated on `gpt-5.5`,
> §5); **phase 1** inline-summarizer fidelity (lifted the 2048 cap → tunable
> knobs); and **phase 3** the **proactive pre-send trigger** (projected =
> last-observed + estimate of items appended since, so an appended item that
> would overflow the next request compacts before it's sent — codex's
> `get_total_token_usage` shape). Model-downshift is covered structurally by the
> existing threshold-on-`set_model` + the reactive check. Only **phase 1b**
> refinements remain (a true transcript *token* budget vs char cap; the
> `BodyAfterPrefix` window scope — likely N/A since brodex compaction genuinely
> shrinks the buffer). Ground truth is `openai/codex` `main` as vendored at
> `/home/invidious/repos/codex` (`codex-rs/…`). Citations are `file:line` into
> that tree and into `crates/bro-harness/`.

## 1. Why this doc exists

bro-harness's brodex transport mirrors codex's Responses wire contract closely
(session/thread ids, `prompt_cache_key`, `service_tier`, `store:false`,
`include:["reasoning.encrypted_content"]`, WS + HTTP framing). Its **compaction**,
however, does not mirror codex: it is a client-side, plaintext, lossy summarize.
Codex's compaction is a **server-side transform** that is structurally different
and materially higher fidelity. This doc captures the codex mechanism precisely
so we can make brodex's compaction OAI-idiomatic rather than a bespoke
approximation.

Scope is the **OpenAI Responses (brodex) path**. The Anthropic and OpenAI-Chat
transports share the same client-side summarizer and the same fidelity gaps, but
they have no server-side compaction endpoint, so their canonical answer is "the
inline fallback, done well" (§6 phase 1). They are out of primary scope here.

## 2. The reference: how codex compaction works

### 2.1 Triggering is proactive (pre-sampling), not reactive

Codex decides to compact **before** it samples the next turn, sized against the
model window — never after a request has already been sent at over-window size.

- `run_pre_sampling_compact` (`core/src/session/turn.rs:711`) runs before every
  turn's model call. It first handles model-downshift (§2.2), then checks
  `auto_compact_token_status` and, if `token_limit_reached`, runs
  `run_auto_compact` with `CompactionReason::ContextLimit`,
  `CompactionPhase::PreTurn`.
- `auto_compact_token_status` (`turn.rs:659`) compares live usage against the
  model's `auto_compact_token_limit`, under one of two **scopes**
  (`AutoCompactTokenLimitScope`):
  - `Total` — all tokens in the active context.
  - `BodyAfterPrefix` — tokens since the current auto-compact *window* started
    (`active_context_tokens - prefill`), so a large stable cached prefix doesn't
    keep re-triggering compaction. It also enforces a hard
    `full_context_window_limit_reached` ceiling.
- The **auto-compact window** (`core/src/state/auto_compact_window.rs`) tracks an
  `ordinal` (incremented per compaction) and a `prefill_input_tokens` baseline
  (server-observed when available, else estimated). `BodyAfterPrefix` measures
  growth *after* the prefix so repeated compactions each get a fresh budget.

Reasons (`CompactionReason`): `UserRequested` (`/compact`), `ContextLimit`,
`ModelDownshift`. Phases (`CompactionPhase`): `StandaloneTurn`, `PreTurn`,
`MidTurn`. Triggers (`CompactionTrigger`): `Manual`, `Auto`.

### 2.2 Model-downshift compaction

`maybe_run_previous_model_inline_compact` (`turn.rs:737`) compacts when switching
to a model with a smaller context window, using the **previous** model's window
so the carried-over history fits the new one. Runs with
`CompactionReason::ModelDownshift` before the first turn on the new model.

### 2.3 Three implementations and the routing decision

`run_auto_compact` (`turn.rs:789`) selects the implementation:

```
should_use_remote_compact_task(provider)            // provider.supports_remote_compaction()
  ├─ true  + Feature::RemoteCompactionV2  → run_inline_remote_auto_compact_task_v2   (streaming)
  ├─ true  (legacy)                       → run_inline_remote_auto_compact_task       (unary endpoint)
  └─ false                                → run_inline_auto_compact_task              (local summarizer)
```

(`should_use_remote_compact_task` = `compact.rs:66`.) The provider capability
gate is the load-bearing idea: **server-side when the backend supports it, local
inline otherwise.** This maps cleanly onto brodex's existing auth-mode routing
(ChatGPT-OAuth backend ⇒ supports remote; generic API-key vendors ⇒ don't).

### 2.4 The server-side transform

Both remote paths push the **real, structured** history to the backend and get
back compacted `ResponseItem`s — no client-side plaintext rendering, no
client-imposed summary-length cap.

**Legacy unary endpoint** (`codex-api/src/endpoint/compact.rs`, path
`responses/compact`): POST a `CompactionInput` and receive
`{ output: Vec<ResponseItem> }`.

```rust
// codex-api/src/common.rs:25
pub struct CompactionInput<'a> {
    pub model: &'a str,
    pub input: &'a [ResponseItem],      // the full structured history
    pub instructions: &'a str,          // base/system instructions
    pub tools: Vec<Value>,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub service_tier: Option<&'a str>,
    pub prompt_cache_key: Option<&'a str>,
    pub text: Option<TextControls>,
}
```

The server applies its retained-message default (64k tokens, see §2.5) and
returns the new history.

**v2 streaming** (`core/src/compact_remote_v2.rs`, the current reference per its
own comment at `:48-49`): instead of a separate endpoint, append a sentinel item
to the normal Responses stream and read back exactly one compacted item.

- Build the prompt input = full history + a trailing
  `ResponseItem::CompactionTrigger` (`compact_remote_v2.rs:208-209`). On the wire
  that item is simply `{"type":"compaction_trigger"}` with no payload
  (`protocol/src/models.rs:897`, round-trip test at `:2524`).
- Stream it through the *same* `client_session.stream(...)` used for normal turns
  (`compact_remote_v2.rs:304`) — so it rides the WebSocket or HTTP path
  identically.
- `collect_compaction_output` (`compact_remote_v2.rs:364`) consumes the stream
  and requires **exactly one** `ResponseItem::Compaction { encrypted_content }`
  output item, returning it plus the `response_id`.

The summary is therefore an **encrypted, opaque `Compaction` item**
(`models.rs:894`, `#[serde(alias = "compaction_summary")]`), not plaintext. This
is the OAI-idiomatic shape and is `store:false`-native — exactly like the
`reasoning.encrypted_content` items brodex already round-trips
(`responses_common.rs:335` handles those today).

### 2.5 History rebuild and verbatim retention

`build_v2_compacted_history` (`compact_remote_v2.rs:409`):

1. From the pre-compaction `prompt_input`, **retain only** `user` / `developer` /
   `system` messages (`is_retained_for_remote_compaction_v2`, `:425`) — assistant
   turns and function call/outputs are dropped (the summary subsumes them).
2. Truncate that retained set to a **token budget**, newest-first, splitting the
   boundary message if needed (`truncate_retained_messages_for_remote_compaction`,
   `:433`). The budget is `RETAINED_MESSAGE_TOKEN_BUDGET = 64_000`
   (`:50`) — "mirror the current /responses/compact retained-message default."
3. Append the single `Compaction` summary item.

So the canonical retained tail is **64k tokens of verbatim user/developer/system
context**, with the summary covering the rest. Then `process_compacted_history`
applies initial-context injection and `replace_compacted_history` +
`recompute_token_usage` install it.

`InitialContextInjection` (`compact.rs:60`): `BeforeLastUserMessage` (mid-turn —
re-inject env/system just above the last real user message, because the model is
trained to see the summary as the last item) vs `DoNotInject` (pre-turn/manual —
the next normal turn re-injects context anyway).

### 2.6 Fit-trimming: make the compaction request itself fit

Before a remote compaction, `trim_function_call_history_to_fit_context_window`
(`compact_remote.rs:376`) removes trailing **codex-generated** items (function
calls/outputs — `is_codex_generated_item`) one at a time while the estimated
token count exceeds the window. This guarantees the compaction request fits even
when the conversation has already blown past the window — the structural answer
to "the conversation is too big to even summarize." (Our overflow-recovery fix is
the reactive cousin of this; codex does it pre-emptively and structurally.)

### 2.7 The local inline fallback

When the provider doesn't support remote compaction, codex runs the summary as a
**real model turn** in-context (`run_inline_auto_compact_task`, `compact.rs:70`):

- Inject `SUMMARIZATION_PROMPT` (`prompts/templates/compact/prompt.md`) as the
  user input and run a normal turn; the model's output IS the summary.
- `build_compacted_history_with_limit` (`compact.rs:487`) then builds:
  `initial_context` + recent **verbatim user messages** up to
  `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` (`compact.rs:49`, newest-first,
  truncating the boundary message) + the summary message.
- On resume, the summary is framed with `SUMMARY_PREFIX`
  (`prompts/templates/compact/summary_prefix.md`: "Another language model started
  to solve this problem and produced a summary…") so the next model knows how to
  read it.

Even the *fallback* keeps 20k tokens of verbatim user turns and lets the summary
be a full turn (no 2k cap) — both things brodex's current client summarizer does
not do.

### 2.8 Cross-cutting machinery

- **Hooks**: `PreCompact` / `PostCompact` (`hooks/src/events/compact.rs`); a pre
  hook can abort compaction.
- **Analytics**: `CompactionAnalyticsAttempt` records reason/phase/trigger/impl/
  status (`compact.rs:305`).
- **Rollout tracing**: `CompactionCheckpointTracePayload` records the exact
  input vs replacement history (`rollout-trace/src/compaction.rs`).
- **User guidance**: a warning that multiple compactions degrade accuracy
  ("Start a new thread…", `core/tests/suite/compact.rs`).

## 3. Where brodex stands today (gap analysis)

| Dimension | Codex (canonical) | brodex today | Gap |
|---|---|---|---|
| Trigger timing | Proactive, pre-sampling, window-sized | Reactive: `last_prompt_tokens > 0.75·window` checked at next step (`agent_loop.rs:526`) | One step late; can't size before sending |
| Overflow safety net | `trim…to_fit` pre-emptively | **Landed**: typed `ContextWindowExceeded` → compact + retry once (`agent_loop.rs`, `transport/mod.rs`, `responses_common.rs`) | Reactive cousin; good enough as a net |
| Scope accounting | Total vs BodyAfterPrefix + window snapshot | Single cache-inclusive `total_input_tokens` (`transport/mod.rs:60`) | No prefix-aware budget; re-compaction can thrash |
| Mechanism | Server-side transform (encrypted `Compaction` item) | Client-side: render prefix → plaintext → one-shot summarize call | Lossy; self-contradictory at over-window sizes |
| Tool-output fidelity | Server sees full structured items | Tool results truncated to **2000 chars** before the summarizer (`responses_common.rs:454`, `openai_chat.rs:576`) | Guts exactly the large outputs that fill the window |
| Summary cap | None (server / full turn) | Responses: none; **OpenAI-Chat: 2048** (`openai_chat.rs:176`); Anthropic: 2048 | Chat/Anthropic squeeze 150k → ~1.5k words |
| Retained tail | 64k tokens, user/dev/system, tail-weighted | `keep_tail = 6` **messages** (`compaction.rs:40`) | Arbitrary unit; a single huge tail message dominates |
| Summary shape | Encrypted `Compaction` item, store:false-native | Plaintext `[Earlier conversation compacted…]` user message (`openai_responses.rs:371`) | Not OAI-idiomatic; replays plaintext |
| Provider gating | `supports_remote_compaction()` | None (always client-side) | No server-side path even when backend supports it |
| Model downshift | Dedicated pre-compaction | `set_model` only re-derives threshold (`agent_loop.rs:462`) | Relies on the reactive net |
| Hooks/analytics/trace/warning | Yes | `compact_boundary` event only (`emit.rs:145`) | Observability only |

What brodex already gets right and should keep: cache-inclusive occupancy
(`total_input_tokens`), split-point safety (`responses_split` never orphans a
`function_call_output`, `responses_common.rs:347`), WS delta-baseline
invalidation on compaction (`openai_responses.rs:380`), and fresh system
recomposition every call (so system grounding is never lost to compaction).

## 4. Target design for brodex

Make the **provider-capability gate** the spine, exactly like codex, and align it
with brodex's existing auth-mode routing:

```
ChatGPT-OAuth backend (supports_remote_compaction) → server-side compaction
                                                      (v2 streaming compaction_trigger,
                                                       reuse the existing WS/HTTP stream)
API-key / generic OpenAI-compatible vendor          → inline summarizer, done well
```

**Server-side path (OAuth) — the canonical target (validated, §5).**
- POST a `CompactionInput` (`{model, input: state.input, instructions, tools,
  parallel_tool_calls}`) to `{http_endpoint}/compact`; parse the returned
  `{output: [...]}` and set `state.input = output`. The server applies retention
  (user/developer/system verbatim, 64k budget) and returns the replacement
  history with a trailing `compaction_summary` (`encrypted_content`) item — no
  client-side rendering, truncation, or summary cap.
- Replay the encrypted `compaction_summary` on the next turn the same way
  reasoning `encrypted_content` is replayed under `store:false` (verified: the
  model answers from it).
- Compaction stays over **HTTP** even when WS is the live turn path (matches
  codex); invalidate the WS delta baseline after rewrite (already done).
- Gate on `Auth::ChatGpt` (the capability proxy, mirroring codex's
  `supports_remote_compaction`); API-key vendors fall through to the inline path.

**Inline path (API-key) — the fallback, done well.**
- Keep the summary a full call (no 2k cap), budget the *transcript* to fit the
  window instead of a flat 2000-char per-result truncation (head+tail / recency
  weighting), and preserve recent **verbatim user messages** up to a token budget
  (codex's 20k) in addition to the summary.
- Replace `keep_tail = 6 messages` with a **token-budgeted** retained tail.

**Triggering (landed, phase 3).**
- Reactive threshold + the overflow recovery remain the safety net.
- **Proactive pre-send estimate (done):** `pending_input_estimate` adds a coarse
  (~chars/4) estimate of items appended since the last observed usage to
  `last_prompt_tokens` for the threshold check, so a step that would overflow the
  next request compacts before it's sent — closing the "one step late" gap.
- **Model-downshift (covered structurally):** `set_model` re-derives the
  threshold; the reactive check then compacts on the next turn when the carried
  history exceeds the new (smaller) window. No dedicated path needed given
  brodex's per-step trigger.

**Policy home.** `summary_max_tokens`, `tool_render_budget`, and
`retained_tail_tokens` should be model-keyed in `compaction.rs` alongside the
existing thresholds (same exact→glob→default resolution), env-overridable.

## 5. Live-probe findings (RESOLVED, 2026-06-02)

Probed against the real ChatGPT backend (account `3212e60e…`, model `gpt-5.5`)
via a double-gated `#[ignore]` test (`responses_common.rs::probe_responses_compaction`,
run with `BRO_HARNESS_LIVE_PROBE=1`). bro-harness's existing identity/auth headers
(no attestation) are accepted as-is — the probe reused `resolve_auth` /
`http_endpoint` / `identity_auth_headers` verbatim.

1. **Unary `responses/compact` — WORKS (200).** URL is `http_endpoint + "/compact"`
   (= `https://chatgpt.com/backend-api/codex/responses/compact`). Request body is
   the `CompactionInput` shape (`{model, input, instructions, tools, parallel_tool_calls}`,
   non-streaming). Response is a single JSON object:
   ```json
   {"object":"response.compaction",
    "output":[ {"type":"message","role":"user","content":[{"type":"input_text",...}]},
               {"type":"message","role":"user",...},
               {"type":"compaction_summary","encrypted_content":"gAAAAA…"} ]}
   ```
   So the **server does the retention + rebuild** and returns the full replacement
   `input[]`: retained messages followed by one `compaction_summary` item. The
   client just sets `state.input = output`.
2. **Retention shape (answers old Q3).** With a `[user, assistant, user]` history,
   the output kept **both user messages verbatim** and **dropped the assistant**
   message — exactly codex's `is_retained_for_remote_compaction_v2`
   (user/developer/system only). The 64k token budget is applied server-side; the
   client does not need to truncate.
3. **Summary item is `compaction_summary` with `encrypted_content`** — the wire
   form of codex's `ResponseItem::Compaction` (`#[serde(alias="compaction_summary")]`).
   Opaque blob, `store:false`-native.
4. **Replay — WORKS (200), and the model uses it (answers old Q2).** Feeding the
   unary output (including the encrypted `compaction_summary`) back as `input[]`
   plus a fresh user turn returned 200 and the model answered correctly from the
   compacted context ("Magic token: BANANA-7; 17*3 = 51"). The encrypted summary
   replays under `store:false` exactly like reasoning `encrypted_content`.
5. **Streaming `compaction_trigger` — WORKS (200).** Appending
   `{"type":"compaction_trigger"}` to a normal `/responses` stream is accepted with
   **no special beta/feature header** (answers old Q4). This is the alternative to
   the unary endpoint and is WS-capable, but it is more complex (turn-metadata
   header handling, stream collection of one item).

**Conclusion: adopt the unary `responses/compact` endpoint** for brodex's
ChatGPT-OAuth path. It is the simplest faithful mechanism — one POST, server-side
retention + encrypted summary, replace `state.input` with the returned `output`.
It is HTTP-only (matches codex's "compaction always over HTTP" and the existing
WS-baseline invalidation), so WS/HTTP parity (old Q5) is a non-issue. The
streaming-trigger path is kept on file as a future option but is not needed.

Remaining to verify during implementation: behaviour on a **large** history with
`function_call`/`function_call_output` items (does the server need the live
`tools` to process them, and does it correctly drop tool-call pairs?) and on a
history that itself already exceeds the window (fit-trim parity).

## 6. Phasing

- **Phase 0 — landed.** Context-window overflow → compact + retry safety net
  (commit `77d0514`).
- **Phase 1 — LANDED (`5038ab8`).** Lifted the inline summary cap (2048 → 8192
  default) and made it + the per-tool-result render cap tunable via
  `CompactionParams` / env, sourced from `CompactionPolicy::params()`. Applies to
  all three transports' inline path (server-side `responses/compact` ignores
  them). Still open as phase 1b if needed: a true transcript **token** budget
  (vs flat char cap) and token-budgeted verbatim-tail retention (vs `keep_tail`
  message count).
- **Phase 2 — LANDED (`35eb59c`), live-validated.** Unary `responses/compact`:
  `build_compaction_input` → POST → replace `state.input` with the returned
  `output` (retained msgs + encrypted `compaction_summary`), gated on
  `Auth::ChatGpt`; `tools` threaded through `Transport::compact`; client-side
  summarizer kept as the API-key fallback. Two double-gated live probes verify
  the endpoint and the real `compact()` path. The streaming `compaction_trigger`
  path remains a documented future option, not required.
- **Phase 3 — LANDED.** Proactive trigger: `Session.pending_input_estimate`
  accumulates a coarse (~chars/4) estimate of items appended since the last
  observed usage (tool results via `est_tool_results`, mid-turn inputs + the new
  user message via `est_tokens` in `push_user_text`), reset on each model call
  and on compaction. The trigger checks `last_prompt_tokens + pending_input_estimate
  > threshold`, mirroring codex's `get_total_token_usage` (last observed +
  estimate of items after the last model turn). Model-downshift is covered
  structurally: `set_model` re-derives the threshold and the reactive check
  compacts on the next turn if the carried history exceeds the new window.
- **Phase 1b (open, optional)** — true transcript *token* budget (vs flat char
  cap) and token-budgeted verbatim-tail retention (vs `keep_tail` count); the
  `BodyAfterPrefix` window scope (likely N/A — brodex compaction genuinely
  shrinks the buffer, so re-compaction thrash isn't a real risk).
- **Phase 4 (optional) — parity: pre/post-compact hooks, analytics, rollout trace.**

## 7. Reference index

Codex (`/home/invidious/repos/codex/codex-rs/`):
- `core/src/compact.rs` — prompts, `should_use_remote_compact_task`,
  inline path, `build_compacted_history_with_limit`, `COMPACT_USER_MESSAGE_MAX_TOKENS`.
- `core/src/compact_remote.rs` — unary endpoint path; `trim_function_call_history_to_fit_context_window`.
- `core/src/compact_remote_v2.rs` — streaming `compaction_trigger` path;
  `build_v2_compacted_history`; `RETAINED_MESSAGE_TOKEN_BUDGET`; `collect_compaction_output`.
- `core/src/session/turn.rs` — `run_pre_sampling_compact`, `auto_compact_token_status`,
  `maybe_run_previous_model_inline_compact`, `run_auto_compact`.
- `core/src/state/auto_compact_window.rs` — window/prefill tracking.
- `codex-api/src/endpoint/compact.rs`, `codex-api/src/common.rs` — `responses/compact`, `CompactionInput`.
- `protocol/src/models.rs:894` — `Compaction` / `CompactionTrigger` items.

bro-harness (`crates/bro-harness/src/`):
- `compaction.rs` — model-keyed `CompactionPolicy`, `COMPACTION_INSTRUCTION`, `keep_tail`.
- `agent_loop.rs` — threshold trigger + overflow recovery in `user_turn`; `compact_manual`.
- `transport/mod.rs` — `Transport::compact`, `Usage::total_input_tokens`, `ContextWindowExceeded`.
- `transport/responses_common.rs` — `responses_split`, `render_responses_transcript`, `parse_sse` error classification.
- `transport/openai_responses.rs` — the unified Responses transport's `compact` + `summarize_text`.

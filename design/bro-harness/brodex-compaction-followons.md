---
title: "Brodex compaction — follow-ons (phase 1b + phase 4)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - providers
  - brodex
  - compaction
brief: "Optional follow-on work split out of the now-archived brodex-compaction canonical design. The canonical set (phases 0-3: overflow recovery, server-side responses/compact for OAuth, inline-summarizer fidelity, proactive trigger) is landed and live-validated. This doc tracks the remaining non-canonical refinements: phase 1b (a true transcript token budget vs flat char cap, token-budgeted verbatim-tail retention, and an assessment of codex's BodyAfterPrefix window scope) and phase 4 (pre/post-compact hooks, analytics, and rollout-trace parity). None of these are gaps in the canonical mechanism; they are improvements to pursue only if the need is observed."
---

# Brodex compaction — follow-ons (phase 1b + phase 4)

> **Status: proposed (optional).** Split out of
> `design/bro-harness/brodex-compaction.md` (now **archived** — the canonical
> set, phases 0-3, is landed). Nothing here is required for codex-canonical
> behavior; these are refinements. Ground truth remains `openai/codex` `main`
> vendored at `/home/invidious/repos/codex` (`codex-rs/…`). See the archived doc
> for the full mechanism, gap analysis, and live-probe findings.

## Phase 1b — inline-summarizer fidelity, deeper

The landed phase 1 lifted the inline summary cap (2048 → 8192 default) and made
the summary budget + per-tool-result render cap tunable (`CompactionParams`,
`compaction.rs`). Two structural refinements remain on the **inline** path
(Anthropic / OpenAI-Chat / the OpenAI-Responses API-key fallback — the
server-side `responses/compact` OAuth path is unaffected, since the backend owns
retention there).

> Scope note: the Anthropic inline transport is another agent's lane (canonical
> Anthropic compaction). Any 1b change touching the Anthropic summarizer must be
> coordinated; the shared pieces (`CompactionParams`, `compaction.rs` policy,
> `agent_loop.rs` trigger) are the safe surface.

### 1b.1 Transcript token budget (vs flat per-result char cap)

Today the prefix transcript is built with a **flat per-tool-result char cap**
(`tool_render_cap`, default 2000). This is safe but crude: raising the cap to
preserve large outputs risks pushing the *summarization request itself* over the
window, while keeping it low guts exactly the big outputs that filled the
context. Codex sidesteps this entirely by compacting server-side over the full
structured history.

Proposed: budget the **whole rendered transcript** to a token target rather than
capping each result independently — e.g. recency-weighted allocation (newer
turns get more budget) and head+tail truncation of individual large outputs
(keep the start and end, elide the middle). This preserves more signal per token
and bounds the summarization prompt deterministically.

### 1b.2 Token-budgeted verbatim tail (vs `keep_tail` message count)

`keep_tail` is a **message count** (default 6). A single huge tail message (a
50k-token tool result) can dominate the post-compaction buffer, while six tiny
messages preserve almost nothing. Codex retains by **tokens**, not count:
`RETAINED_MESSAGE_TOKEN_BUDGET = 64_000` for the server path
(`compact_remote_v2.rs:50`) and `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000` for
the inline path (`compact.rs:49`).

Proposed: replace the `keep_tail` count with a token budget in
`CompactionParams`, and have each inline transport's split walk newest-first
until the budget is spent (preserving the existing tool-call/result pairing
safety in `responses_split` / `chat_split` / the Anthropic assistant-boundary
search).

### 1b.3 `BodyAfterPrefix` window scope — assessment (likely N/A)

Codex's `auto_compact_token_status` supports a `BodyAfterPrefix` scope backed by
an auto-compact window that tracks a prefill baseline
(`state/auto_compact_window.rs`), measuring growth *after* a large stable cached
prefix so a cache-heavy session doesn't re-trigger compaction every turn.

Assessment: **probably not needed for brodex.** brodex compaction genuinely
shrinks the buffer — the inline path rebuilds `[summary] + tail`, and the
server-side path returns a retained-tail + summary — so `total_input_tokens`
drops after a compaction and the threshold is not immediately re-crossed. Codex
needs `BodyAfterPrefix` partly because of how its windows/prefill accounting
works; brodex's post-compaction buffer is small by construction. Revisit only if
re-compaction thrash is actually observed on a cache-heavy session; if so, the
window/prefill machinery in `auto_compact_window.rs` is the reference.

## Phase 4 — hooks / analytics / rollout-trace parity

Codex wraps compaction in cross-cutting machinery brodex does not have:

- **Pre/post-compact hooks** (`hooks/src/events/compact.rs`): a `PreCompact` hook
  that can abort compaction, and a `PostCompact` hook. brodex has no compaction
  hook surface.
- **Analytics** (`CompactionAnalyticsAttempt`, `compact.rs:305`): records
  reason / phase / trigger / implementation / status per attempt. brodex emits a
  single `compact_boundary` stream-json event (`emit.rs`) — enough for basic
  observability, nothing structured.
- **Rollout tracing** (`CompactionCheckpointTracePayload`,
  `rollout-trace/src/compaction.rs`): records the exact input vs replacement
  history at the compaction checkpoint.

Assessment: **lower priority.** The `compact_boundary` event already gives the
operator a visible signal (trigger, pre-tokens, summary size). Pursue parity only
if compaction observability/debuggability becomes a real need; it is not part of
the canonical compaction *mechanism*.

## Pointers

- Archived canonical design: `design/bro-harness/brodex-compaction.md`.
- Policy + knobs: `crates/bro-harness/src/compaction.rs`
  (`CompactionPolicy`, `CompactionParams` via `transport::CompactionParams`).
- Trigger: `crates/bro-harness/src/agent_loop.rs`
  (`pending_input_estimate`, `est_tokens`, `est_tool_results`).
- Inline summarizers: `transport/anthropic.rs`, `transport/openai_chat.rs`,
  `transport/openai_responses.rs` (`summarize_text`, `render_*_transcript`).

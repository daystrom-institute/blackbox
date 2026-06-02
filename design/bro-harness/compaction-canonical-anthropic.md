---
title: "Canonical compaction model — Anthropic-shaped consumers"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - bro-harness
  - compaction
  - providers
brief: "A canonical model for context compaction on the Anthropic transport (Claude Messages, and the Anthropic-compatible GLM/DeepSeek endpoints) in bro-harness, reverse-derived from the Claude Code binary. Covers the summarizer prompt, the trigger/threshold math, and the operational guardrails, then maps them onto the as-built harness. OAI Responses compaction is out of scope and owned by a separate doc; this doc only marks the shared bro-harness seams where the two meet."
---

> **Scope.** This doc is **Anthropic-shaped only**: the Claude Messages transport
> and the Anthropic-compatible endpoints bro-harness drives through it (`glm-*`,
> `deepseek-*`, `claude-*`). It does **not** design OAI Responses compaction —
> that is owned by a separate canonical doc. The only place OAI appears here is
> as **seam callouts** (§6): the bro-harness abstractions (`CompactionPolicy`,
> the `agent_loop` trigger, the `Transport::compact` boundary) are shared across
> transports, so this doc marks where the OAI doc plugs in, without designing it.

## 1. Why this doc exists

bro-harness already compacts (`crates/bro-harness/src/compaction.rs`,
`agent_loop.rs`, `transport/anthropic.rs::compact`). What it lacks is a
*reference* for what good compaction looks like for an Anthropic-shaped API
consumer — the prompt, the trigger heuristic, and the guardrails — so we can
judge the as-built behavior against a known-good baseline and close the gaps
deliberately rather than by guess.

The most mature Anthropic-transport consumer available is **Claude Code itself**.
It is proprietary and not open source — the public `anthropics/claude-code` repo
is issues/docs/plugins, not source — but the shipped binary is a Bun-compiled
single file with the JS bundle (and its string literals) embedded as plaintext.
The compaction prompts and the trigger logic are recoverable with `strings` +
`grep`.

### 1.1 Provenance / reliability

- **Source:** local install `~/.local/share/claude/versions/2.1.160` (arm64
  Mach-O, ~207 MB). Extraction: `strings -n 1` over the binary, then grep.
- **High confidence — verbatim string literals:** both summarizer prompts (§3)
  and the continuation wrapper are reproduced exactly as they appear in the
  bundle.
- **Medium confidence — decoded minified logic:** the threshold math (§4) is
  read out of minified JS with mangled identifiers (`rN_`, `tqH`, `mg8`, …).
  The *constants and structure* are reliable; the exact call graph is a
  best-effort decode and should be treated as "how CC appears to do it," not a
  spec we owe bug-for-bug compatibility with.
- **Legal note:** this is reverse-engineering a licensed binary for interop
  understanding. Use it to inform our own design; do not paste the proprietary
  prompt text verbatim into shipped harness code. The structure and the
  heuristics are what we adopt.

## 2. The shape of compaction (Anthropic transport)

The Anthropic Messages API is **stateless**: every turn resends the full
`messages` array, and the server holds no conversation state. Compaction is
therefore a purely client-side operation — rewrite the local message buffer so
it occupies fewer tokens while preserving enough to continue:

1. Pick a **split point** in the message history.
2. **Summarize** the prefix `[..split]` into one synthetic message.
3. **Rebuild** the buffer as `[summary, ...tail]` and continue.

Everything below is detail on (a) *when* to trip this, (b) *how* to prompt the
summary, and (c) the *invariants* the rebuild must not violate.

## 3. The summarizer prompt

Claude Code ships **two** summarizer prompts, selected by situation. Both share
one section skeleton; the framing and the final two sections differ.

### 3.1 Variant A — partial / "continuing session"

Used when real messages will follow the summary (the summary is prepended and
the session continues live). Framing (paraphrased): *"This summary will be placed
at the start of a continuing session; newer messages that build on this context
will follow after your summary (you do not see them here)."* Its final sections
are retrospective:

- **8. Work Completed**
- **9. Context for Continuing Work**

### 3.2 Variant B — full / "ran out of context"

The classic hard-compaction prompt: *"create a detailed summary of the
conversation so far … essential for continuing development work without losing
context."* Its final sections are forward-looking:

- **8. Current Work**
- **9. Optional Next Step** — and Next Step **demands verbatim quotes** of where
  work left off, *"to ensure there's no drift in task interpretation."*

### 3.3 Shared section skeleton (the part worth adopting)

Both prompts ask for a `<analysis>` scratchpad followed by a `<summary>` with:

1. **Primary Request and Intent**
2. **Key Technical Concepts**
3. **Files and Code Sections** — full snippets + *why each matters*
4. **Errors and fixes** — including the user's feedback on the error
5. **Problem Solving**
6. **All user messages** — every non-tool-result user message; "critical for
   understanding feedback and changing intent"
7. **Pending Tasks**
8. **Current Work / Work Completed**
9. **Optional Next Step / Context for Continuing Work**

### 3.4 Three properties to replicate

These are the design decisions baked into CC's prompt that our one-line
`COMPACTION_INSTRUCTION` currently lacks:

- **`<analysis>` scratchpad before the summary.** Forces a chronological pass
  and an explicit accuracy/completeness self-check before emitting.
- **Verbatim security-constraint preservation (correctness, not nicety).** The
  prompt mandates: *"Note any security-relevant instructions or constraints …
  These MUST be preserved verbatim … so they continue to apply after
  compaction."* Compaction must not silently drop a "never touch X" rule. This
  is a real safety property of the summarizer, and it complements the harness's
  own constraint of keeping security-relevant user text intact across the cut.
- **Anti-drift via verbatim quotes** (Variant B Next Step) — quote the last
  task state instead of paraphrasing it.

## 4. The trigger / threshold algorithm

Decoded from the minified bundle (identifiers mangled; constants reliable).

### 4.1 Effective window

Output tokens are reserved out of the context window *first*:

```
effectiveWindow = contextWindow − min(maxOutputTokens, 20_000)
```

`maxOutputTokens` ← `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, capped at 20 000 for the
purposes of this reservation (`O0K = 20000`).

### 4.2 Two thresholds

```
compactThreshold  = effectiveWindow − 13_000          # fixed headroom
blockingThreshold = min(effectiveWindow − round(effectiveWindow * 0.20),
                        compactThreshold)
```

- **`used ≥ compactThreshold` → level "compact"** — trip a compaction.
- **`used ≥ blockingThreshold` → level "blocked"** — hard floor; refuse to
  proceed rather than overflow.

**The headline finding: the default trigger is a fixed ~13k-token headroom, not
a percentage.** A `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` exists but is a *test*
override (`testPctOverride`); the production path is the fixed headroom. A fixed
buffer behaves more predictably than a ratio across very different window sizes
(200k vs 1M).

### 4.3 Guardrails around the trigger

- **Precompute-and-swap.** CC computes the compaction in the background as it
  *approaches* the threshold and swaps it in atomically
  (`lastTransitionReason === "precomputed_compact_swap"`), so the user doesn't
  eat a synchronous summarization stall at the wall. `precomputeBufferFraction`
  default `0.20` (statsig-tunable) controls how early.
- **Anti-thrash.** If a turn just compacted and the turn counter is still low
  (`< 3`), CC tracks `consecutiveRapidRefills` to avoid a compaction loop when a
  *single* turn keeps blowing the budget.
- **Strip-non-essential first.** Compaction can `stripNonEssential` (drop old
  tool outputs) as a cheaper tier before paying for a full LLM summarization
  pass.
- **Telemetry.** `tengu_compact` records `preCompactTokenCount`,
  `postCompactTokenCount`, `truePostCompactTokenCount`, `stripNonEssential`,
  `willRetriggerNextTurn`, `isAutoCompact`.
- **Disable switches.** `DISABLE_COMPACT`, `DISABLE_AUTO_COMPACT`,
  `CLAUDE_CODE_COLD_COMPACT`.

### 4.4 Continuation wrapper

On the *next* session, the summary is prefixed with: *"This session is being
continued from a previous conversation that ran out of context. The summary
below covers the earlier portion of the conversation."* Custom `/compact <hint>`
text is appended as an **"Additional Instructions:"** block on the summarizer
prompt.

## 5. Mapping onto bro-harness as-built

The harness already has the right *layering*; the gaps are in prompt richness and
a few of the guardrails.

| Canonical (CC) | bro-harness today | Gap |
| --- | --- | --- |
| Effective window = window − reserved output | `CompactionPolicy::threshold` = `window * compact_at` (default 0.75) | No output reservation; ratio not headroom |
| Fixed 13k headroom trigger | `compact_at` ratio (0.75 default), model-keyed | Ratio is fine, but no fixed-headroom option; no blocking floor |
| Trigger on used tokens | `agent_loop`: `last_prompt_tokens > compact_threshold` at top of turn loop; uses `Usage::total_input_tokens` (cache-inclusive) | Equivalent altitude ✓ |
| Structured 9-section prompt + `<analysis>` + verbatim security constraints | One-line `COMPACTION_INSTRUCTION` | **Biggest gap** — adopt the structured prompt |
| Two prompt variants (partial vs full) | Single instruction | Harness compaction is always mid-session (more messages follow) → wants Variant **A** framing |
| Precompute-and-swap | Synchronous compact at top of turn | Stall on compaction; could precompute |
| Anti-thrash (`consecutiveRapidRefills`) | None | A single oversized turn can re-trip every turn |
| Strip-non-essential tier | None — always summarizes | Could drop stale tool results first |
| Continuation wrapper | `[Earlier conversation compacted to a summary]\n\n{summary}` prepended as a synthetic `user` message | Equivalent intent ✓ |
| Boundary telemetry | `emit.rs::compact_boundary` (`trigger`, `pre_tokens`, `summary_chars`) | Lighter than `tengu_compact` but adequate |

### 5.1 The as-built Anthropic mechanism

`transport/anthropic.rs::compact(keep_tail, instruction, opts)`:

1. No-op if `messages.len() ≤ keep_tail + 1`.
2. **Split on an assistant boundary.** Search backwards from the `keep_tail`
   boundary for the newest `assistant` message and split there. This is the
   load-bearing invariant: it guarantees that after prepending one synthetic
   `user(summary)` message the buffer alternates validly **and never orphans a
   `tool_result` whose matching `tool_use` landed in the discarded prefix.**
   The Anthropic Messages API rejects an orphaned `tool_result`, so this is a
   correctness constraint, not an aesthetic one.
3. `render_transcript(prefix)` → `summarize_text(transcript, instruction, opts)`.
4. Rebuild as `[user(summary), ...messages[split..]]`.

This split-on-assistant rule is **Anthropic-message-shape-specific** and is the
kind of thing the OAI doc will have its own version of (its block model is
reasoning-items + function-call/output pairs, not user/assistant + tool_use/
tool_result).

## 6. Seam callouts — where this meets the OAI doc

> These are the **only** OAI-touching notes in this doc. They exist so the two
> canonical docs compose at the shared bro-harness abstractions instead of
> overlapping or contradicting. The OAI doc owns everything behind these seams.

- **`CompactionPolicy` (`compaction.rs`) is shared and must stay
  transport-neutral.** It keys on the `--model` string only (window + ratio
  lookup with `exact → longest-glob → default` fallback). Both transports get
  their thresholds here. Any Anthropic-specific tuning we add (e.g. a
  fixed-headroom mode) must be expressible without assuming Anthropic message
  shape, or it belongs below the `Transport::compact` seam, not in the policy.
- **The trigger in `agent_loop.rs` is shared.** `last_prompt_tokens >
  threshold` runs regardless of transport; it reads `Usage::total_input_tokens`
  (`transport/mod.rs`). The OAI doc must confirm its `Usage` is populated with
  the same semantics (Responses `input_tokens` incl. cached) so the shared
  trigger fires correctly for both — that reconciliation is an OAI-doc
  responsibility, flagged here only so it isn't dropped.
- **`Transport::compact` is the cut point.** The trait method
  (`transport/mod.rs:242`, default no-op) is where the mechanism diverges. This
  doc specifies the `anthropic.rs` impl; the OAI Responses impl
  (`openai_responses*.rs`) is the other doc's subject. **Reasoning-item /
  `encrypted_content` preservation under `store:false` is explicitly OUT OF
  SCOPE here** — it is a Responses-only concern (the Anthropic impl has no
  reasoning items to replay) and the OAI doc owns the design for keeping
  encrypted reasoning items alive across a prefix rewrite.
- **Thinking-block replay is handled separately on the Anthropic side too.**
  Note the Anthropic transport deliberately omits the `interleaved-thinking` /
  `context-management` betas (`DEFAULT_ANTHROPIC_BETAS`), so the current compact
  path has no thinking-block replay obligation. If that changes, the
  split-on-assistant invariant in §5.1 must be revisited (a thinking block must
  not be orphaned from its turn) — but that is an Anthropic-side follow-up, not
  an OAI seam.
- **`emit.rs::compact_boundary` is shared** and transport-neutral; both impls
  should emit it with their own `trigger`/`pre_tokens`.

## 7. Recommended changes (Anthropic path) — status

Reconciled against landed work (this lane + the brodex compaction phases on
`main`). All Anthropic-scoped; none touch the OAI seam.

1. ✅ **LANDED — structured `COMPACTION_INSTRUCTION`** (Variant A framing,
   9-section skeleton, `<analysis>` scratchpad, mandatory verbatim preservation
   of security-relevant user instructions) plus a shared `transport::extract_summary`
   that keeps only the `<summary>` block. Live-validated on GLM + DeepSeek (all
   9 sections, clean extraction, security constraint preserved verbatim).
2. 〜 **Partially landed — summary budget.** brodex phase 1 lifted the summary cap
   (2048 → 8192, tunable) and added a per-tool-result render cap (`CompactionParams`).
   The deeper *token-budgeted* transcript + verbatim tail (vs char cap / message
   count) is tracked in `brodex-compaction-followons.md` (1b) — the shared/OAI lane.
3. ✅ **Predictive trigger — LANDED as brodex phase 3.** The proactive trigger
   compacts on `last_prompt_tokens + pending_input_estimate` before an overflowing
   request is sent. A hard *blocking floor* + `count_tokens` pre-probe remain an
   optional residual (R1 in `bro-harness-residuals.md`).
4. 〜 **Anti-thrash** — partly covered: a genuine post-compaction buffer is small
   by construction (`[summary] + tail`), so re-compaction rarely retriggers (see
   the brodex follow-ons 1b.3 assessment). No dedicated guard yet; revisit only if
   thrash is observed.
5. ○ **Strip-non-essential tier** (optional) — not implemented; the structured
   summarizer + render cap cover the need today.
6. ○ **Precompute-and-swap** (optional, larger) — not implemented; only worth it
   if the synchronous summarization stall is observably hurting the Fleet TUI UX.

## 8. Validation

- Unit: extend `compaction.rs` tests for any new reservation/headroom math
  (mirror the existing `threshold_applies_ratio_and_respects_disable` style).
- Transport: `anthropic.rs::compact` already has the message-validity invariant;
  any prompt change must keep a test asserting the rebuilt buffer alternates and
  carries no orphaned `tool_result`.
- Behavioral: drive a real over-window session through bro-harness and confirm
  the emitted `compact_boundary` marker, the post-compact token drop, and that
  the next turn continues coherently. Per project convention, validate
  user-facing Fleet TUI rendering of the boundary divider with tmux MCP against a
  live session, not only unit tests.
- Do **not** assert against the prod daemon or real `$HOME`; use isolated state.

## 9. References

- `crates/bro-harness/src/compaction.rs` — `CompactionPolicy`, model-keyed
  thresholds, `COMPACTION_INSTRUCTION`.
- `crates/bro-harness/src/agent_loop.rs` — auto + manual `/compact` triggers.
- `crates/bro-harness/src/transport/mod.rs` — `Transport::compact` seam,
  `Usage::total_input_tokens`.
- `crates/bro-harness/src/transport/anthropic.rs` — the Anthropic compact
  mechanism (split-on-assistant rebuild).
- `crates/bro-harness/src/emit.rs` — `compact_boundary` stream marker.
- `design/bro-harness/anthropic-harness.md` — transport/agent-loop as-built.
- `design/bro-harness/brodex-responses-deep-dive.md` — Responses transport
  context (the OAI compaction doc is the authority for that path).
- Mined: Claude Code `2.1.160` binary (Anthropic-transport consumer) — prompts
  and threshold heuristics, §3–§4.

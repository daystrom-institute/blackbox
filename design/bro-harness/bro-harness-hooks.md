---
title: "bro-harness hooks & nudges (the ambient-meta seam)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "An internal, harness-owned interception subsystem for the bro-harness agent loop. Named hook points observe loop state (user turn, assistant turn, tool result) and contribute ambient meta — the first consumer being a Nudger that surfaces blackbox atoms/tools/system-memories when behavioral or lexical triggers fire. Separates what is injected (a nudge) from where it lands (delivery mechanism), and depends on a static/volatile split of the system prompt for cache safety."
---

> **As-built record.** §1 (system-prompt static/volatile split), §2 (hook
> scaffold + gating ledger), §3 (delivery mechanisms), and the Nudger v1 engine
> with four shipped rules are all built. The v2 catalog-metadata channel
> (`nudge_triggers` on atom/SM/tool descriptors → daemon-compiled blob) and the
> broader rule set were excised to
> [`backlog-hooks-catalog-metadata.md`](./backlog-hooks-catalog-metadata.md) —
> deliberately gated on the §6 adoption loop showing the engine earns its keep.

# bro-harness hooks & nudges (the ambient-meta seam)

> **Status — partial (implemented).** Built and merged: §1 (system-prompt
> cache split), §2 (hook scaffold + gating ledger), §3 (four shipped rules),
> and the adopt-or-explain gap-note directive. **Not built:** §4 catalog-metadata
> channel (gated on adoption data). This doc is reconciled to the as-built code
> in `crates/bro-harness/src/{hooks.rs,agent_loop.rs,transport/*}`; where an
> earlier draft's sketch differed from what shipped, the §2/§3 text below has
> been corrected to match the code, not the original proposal.

## Problem

We control the harness, so we can steer the model toward our own rich toolbox
(refactor atoms, `bbox_slice_*`, `bbox_hybrid_search`/`code_nav`, orchestration
primitives) instead of letting it fall back to generic built-ins. The lever is
**ambient guidance** the harness contributes at the right moment — *not* taking
tools away, *not* rejecting calls, just nudging. Two shapes:

- **One-time signposting** — fire once when a condition is first observed.
  "If you are doing refactor work, `bbox_knowledge` sm-refactor first; otherwise
  disregard this."
- **Periodic contextual reminders** — re-assert an ambient hint while a
  condition keeps recurring, debounced. E.g. the agent keeps manually
  copy-pasting file content → keep nudging toward `clip_*`/`bbox_slice_*`.

The working hypothesis (operator observation): when a custom tool is genuinely
superior **and** the agent is nudged toward it, the agent adopts it most of the
time. When a tool is noisy or wrong-shaped, the agent does one perfunctory call
to satisfy the nudge and then falls back to built-ins. That makes adoption a
**measurable signal**, and the nudge subsystem must instrument it (§6) so dead
nudges get pruned on evidence, not vibes.

## Framing correction: not "system messages" — an internal hook subsystem

The early framing was "inject our own system messages." That conflates two
separable things and carries an Anthropic-specific nuance we did not intend:

- **What** is injected — a nudge (ambient meta).
- **Where** it lands — a delivery mechanism, chosen per-nudge and per-transport.

A **hook subsystem** separates them. Named interception points in the agent loop
fire handlers that observe state and emit `Nudge`s; a separate delivery layer
decides whether a given nudge rides an existing `tool_result`, a volatile system
tail, or (OpenAI-only, reserved) a synthetic developer turn. The Nudger is then
just the **first consumer** of the hooks, not the architecture.

### Relationship to the design's "no hooks" non-goal

`anthropic-harness.md` lists **hooks** as an explicit non-goal. That referred to
**Claude Code's user-facing hook UX** — shell-command `PreToolUse`/`PostToolUse`
gates, permission prompts, user-authored config. This doc proposes something
different in kind: an **internal, harness-owned interception seam** with no user
configuration surface and no permission semantics. We are not reneging on the
non-goal; we are adding an internal extension point. If anything blurs that line
later (e.g. exposing hook registration to operators), it must be re-litigated
against that non-goal explicitly.

### This fits the grain of the code

The loop already contains a proto-hook. `tool_search` mutates a shared
`activated: Arc<Mutex<HashSet>>` (`registry.rs:72`) to change what the *next*
turn looks like, and `todos` is a persisted cross-turn ledger seeded from and
flushed back into the session `side` cell (`agent_loop.rs:66`, `:166`). A hook
subsystem generalizes "an observer mutates shared state that outlives the turn"
from *tool availability* and *plan state* to *ambient guidance*. It is the same
pattern the chaining design applies to *data* (the ref store).

## §1 — Prerequisite: split the system prompt into static + volatile

This is a correctness fix and the **first port of call**, nudges or not.

Today `compose_system` (`agent_loop.rs:179`) concatenates three things into one
string: the daemon-supplied system prompt (the static "initial context" — the
brofile lens), the **pinned** tools section (static for the session), and the
**manifest** of deferred tools (volatile — it shrinks as `tool_search` activates
tools, `registry.rs:178`). Then the Anthropic transport stamps
`cache_control: ephemeral` on **that entire string as a single block**
(`anthropic.rs:103-105`). Consequence: on any turn where the manifest changes,
the whole prefix — including the static lens — cache-misses. The static initial
context is hostage to the volatile manifest.

**Fix — emit `system` as a two-block array, boundary drawn at static/volatile:**

```text
system: [
  { text: <daemon system prompt + pinned section>, cache_control: ephemeral },  // never changes mid-session → cached
  { text: <manifest> + <volatile nudges> }                                       // recomposed each turn, uncached, ~free
]
```

The manifest moves *out* of the cached block; volatile nudges (§5) live in the
same uncached tail. Activation and nudges then never invalidate the static
prefix. `compose_system` returns a `(static, volatile)` pair instead of one
string; each transport renders it natively:

- **Anthropic** — two `system` content blocks, `cache_control` on the first only.
- **OpenAI Chat** — `instructions`-equivalent leading system message stays the
  cached prefix; the volatile tail is a trailing `{role:system|developer}`
  message (Chat caches the prefix automatically).
- **OpenAI Responses** — static text in `instructions`; volatile tail as a
  trailing `developer` item in `input[]` (`prompt_cache_key` caches the prefix).

**Honest caveat (not solved here, not worsened here).** Anthropic's cache order
is tools → system → messages, and the tiering scheme grows the **tools array**
on activation, which busts the cache from the tools layer onward regardless of
what `system` does. That is a pre-existing cost the deferral design already
accepted (`anthropic-harness.md`, "Deferred tooling & tiering"). The operative
rule for this design: **nudges must never ride the tools array** (they don't),
and the static-system split is still strictly correct on its own.

## §2 — Hook points

Three observation points on the existing loop, plus a turn-boundary tick for
bookkeeping. All run in-process; evaluating a hook adds **zero** API tokens.

| Hook | Fires | Sees | Primary use |
|---|---|---|---|
| `on_user_turn(prompt)` | before first `run_turn` | the user ask | lexical triggers on the request |
| `on_assistant_turn(out)` | after `run_turn`, before dispatch (`agent_loop.rs:137`) | assistant text + the `tool_calls` about to run | text-shape triggers; pre-dispatch signposts |
| `on_tool_result(call, result)` | inside the dispatch loop (`agent_loop.rs:146-156`), per call | the call **args and** the result | **behavioral triggers** — the rich seam |
| `on_turn_boundary()` | end of each loop iteration | ledger only | cooldown decrement, fired-state housekeeping |

`on_tool_result` is the load-bearing one because it has both sides of a tool
call. "Manual copy-paste" is not a regex over prose — it is the structural fact
that a `file_write`/`file_edit` `new_string` contains a substring of a prior
`file_read`'s output, and the harness holds both server-side. Behavioral
triggers like this are **far** lower false-positive than lexical ones and are
the spine of the Nudger (§4).

**As-built shape (`hooks.rs`).** An earlier draft proposed one
`evaluate(&mut HookCtx)` with the ledger passed into the hook. What shipped is
cleaner and is the authoritative shape: a hook is a **pure, stateless matcher**
with one method per phase, returning *candidate* nudges. It never sees the
ledger. The `HookEngine` owns the ledger and applies the gate/rank/cap policy
centrally (so triggers are trivially unit-testable in isolation):

```rust
pub trait Hook: Send + Sync {
    fn on_user_turn(&self, _prompt: &str) -> Vec<Candidate> { Vec::new() }
    fn on_assistant_turn(&self, _text: &str, _calls: &[ToolCall]) -> Vec<Candidate> { Vec::new() }
    fn on_tool_result(&self, _call: &ToolCall, _result: &ToolResult) -> Vec<Candidate> { Vec::new() }
}

// A Candidate is { rule_id, message, delivery, kind, priority }.
// The engine: collects candidates from every hook for the phase →
//   admit(): rank by priority (desc), apply the ledger gate
//   (Signpost: try_fire_once; Periodic: cooldown), cap to ONE per phase,
//   append the shared GAP_NOTE_DIRECTIVE → returns Vec<Nudge>.
// HookEngine::tick() is the turn-boundary hook (decrements cooldowns).
```

A behavioral matcher that needs cross-call memory (the copy-paste detector
remembering recent reads) holds its own interior-mutable state inside the hook
struct; the *ledger* stays gating-only.

## §3 — Delivery mechanisms (the persistence trade)

**Delivery (`Delivery`) and recurrence (`NudgeKind`) are independent axes** — a
rule picks each. (An earlier draft coupled them 1:1, rider=signpost /
tail=periodic; the as-built rules do not, so that coupling is corrected here.)

`Delivery` — *where the nudge lands*, chosen by its persistence need:

| Mechanism | Where it lands | Lifetime |
|---|---|---|
| **`Rider`** | appended to an existing `tool_result`'s content (`<harness-note>…</harness-note>`) | **persists** in the transport snapshot; round-trips. Best when the nudge is contextual to a specific action just taken. |
| **`SystemTail`** | §1 block 2 (Anthropic system tail / OpenAI trailing developer msg) | **ephemeral** — recomposed each turn; never accumulates. Best for ambient reminders not tied to one action. |

`NudgeKind` — *how often it may fire*, enforced by the ledger gate:
`Signpost` (once per session) or `Periodic { cooldown }`.

The as-built rules show the axes are orthogonal:

| Rule | Delivery | Kind | Phase |
|---|---|---|---|
| `copy-paste-to-slice` | `Rider` | `Periodic{8}` | tool_result |
| `shell-grep-to-code-search` | `Rider` | `Periodic{6}` | tool_result |
| `refactor-signpost` | `SystemTail` | `Signpost` | user/assistant turn |
| `hedged-convention` | `SystemTail` | `Periodic{10}` | assistant turn |

Note a `Rider` is `Periodic` here, not `Signpost` — a recurring rider is fine
because each lands on a *different* tool_result (it does not pile up on one), and
the cooldown bounds the rate. The original worry — a periodic reminder
*accumulating* — only applies to a nudge that would otherwise repeat in the same
place; `SystemTail` solves that for the ambient ones.

Synthetic developer/system turns (a third, OpenAI-only mechanism) stay in
reserve. We do not need them, and on Anthropic the only mid-array option is a
`user` message — the masquerade we are explicitly avoiding. The rider and the
volatile tail are both honest: framed as harness/environment guidance, which is
exactly what they are. This mirrors Claude Code's own `<system-reminder>`
pattern (tagged, low-salience, attributed to the harness).

## §4 — The Nudger (first consumer)

The Nudger is a set of `Hook`s plus a rule table. A rule:

```text
NudgeRule {
  id,                         // stable; keys the fired-ledger and adoption log
  trigger:    Behavioral | Lexical,   // matcher over HookCtx
  target:     atom | tool | sm-id,    // what we steer toward
  message,                            // ONE line, self-cancelling phrasing
  kind:       Signpost | Periodic,    // → delivery mechanism (§3)
  cooldown_turns,                     // periodic only
}
```

**Prefer behavioral triggers; lexical is the fallback.** Candidate rules,
ordered by trigger quality:

| Trigger (behavioral unless noted) | Steer toward |
|---|---|
| `file_read` output substring reappears in a later `file_write`/`file_edit` | `bbox_slice_*` (server-side move, no context round-trip). *As shipped, `copy-paste-to-slice` targets `bbox_slice_copy`/`bbox_slice_move` only — `clip_*` is designed but unbuilt, so the rule does not yet point at it.* |
| `shell_run` running `grep`/`rg`/`find` over the repo tree | `bbox_hybrid_search` / `bbox_code_query` / code_nav |
| repeated `bro_status` polling, or sequential `bro_exec` calls | `bro_when_all` / `bro_when_any` / `bro_orchestrate_run` |
| N structurally-similar `file_edit` diffs across files | `bbox_refactor_plan` / a refactor atom |
| *(lexical)* user/assistant text matches refactor cues | `bbox_knowledge` sm-refactor — signpost |
| *(lexical)* assistant hedges a convention ("I think we use…") | `bbox_knowledge` |

### Rule source — phased

1. **Harness-shipped defaults first.** A small static table in the harness, so
   the engine and the adoption loop (§6) can be validated before investing in a
   metadata channel. Start with the 2-3 highest-quality behavioral rules
   (copy-paste→slice, shell-grep→code-search) plus the refactor signpost.
   **This shipped (Nudger v1).**
2. **Catalog metadata as v2** — extracted to
   [`backlog-hooks-catalog-metadata.md`](./backlog-hooks-catalog-metadata.md).
   Not built; deliberately gated on the §6 adoption loop showing the engine earns
   its keep.

## §5 — The nudge ledger (state)

A `nudges` cell in the session `side` blob — the plumbing already exists. Both
`Restored.side` (`session.rs:36`) and `SaveState.side` (`session.rs:46`) are
live, and `agent_loop.rs` already seeds `todos` from `side` on open and flushes
it back at save (`:66`, `:166`). The ledger rides the same path:

```text
NudgeLedger {
  fired:    Set<rule_id>,          // one-time signposts never repeat across exec→resume
  cooldown: Map<rule_id, turns_remaining>,  // periodic debounce
}
```

This is **gating state only** — what a rule needs to decide whether to fire
again. There is deliberately no adoption/telemetry record here: whether a nudge
was adopted is a transcript query (§6), not state the harness duplicates.

Seed it next to `todos` at open; flush it next to `todos` at save. No session
signature churn — `side` is exactly the "future durable side-cells don't churn
the signature" widening the clipboard doc introduced.

## §6 — Noise discipline and the adoption loop

The operator's stated worry is noise/context bloat. The rules that keep this a
signal channel, not a progress log:

- **Free when silent.** Hooks are local computation; they add zero tokens when
  nothing fires. Cost when firing is the §1 split (so the static prefix is never
  bust) plus the nudge text itself.
- **One nudge per turn, ranked.** If multiple rules match, emit the single
  highest-priority one. Hard cap.
- **Self-cancelling phrasing.** "If you are *not* doing X, disregard." Cheap for
  the model to discard.
- **Cooldown + fired-ledger.** Nothing repeats inside its window or across
  resume.
- **Adoption is a transcript query, not harness state.** This project *is* a
  complete, indexed log of every tool call ever made (`transcripts/` normalizes
  each event into `NormalizedTranscriptEvent{ tool_call }`; every dispatch is
  bound to a `TranscriptLocation`). The nudge firing is in that same log — the
  `<harness-note>` rider lives in the conversation transcript. So adoption is a
  retrospective **query/projection over the corpus**, not a counter the harness
  maintains. For each session where a rule fired (anchor on the rider text),
  scan the subsequent `ToolUse` events and classify into three outcomes:
  - a steered-toward tool is called → **adopted** (the tool earns its keep);
  - a `bbox_note(kind="followup")` gap note appears → **declined with feedback**
    (actionable signal — see the adopt-or-explain directive below);
  - neither — another manual `shell_run` grep, another verbatim `file_write` →
    **declined silently**, the "called the built-in but filed no gap note" case,
    i.e. the WHY a rule isn't landing.
  Aggregate across sessions → per-rule adopt / feedback / silent rates, run on
  demand via the retrieval surface (`bbox_search` / `bbox_messages` / the graph).
  This is what makes the operator's hypothesis falsifiable and A/B-able, with
  **zero** duplicated telemetry in the harness. (An earlier draft proposed an
  in-harness `(fired, adopted)` counter + `bro_report`; that was a redundant,
  inferior re-implementation of the corpus and was removed.)
- **Adopt-or-explain — the decline path is a gap note.** Every delivered nudge
  carries a shared directive: if the agent declines the steer *because the tool
  is deficient* (buggy, missing a capability, wrong-shaped), it should not
  silently fall back — it should file a tool-surface gap note via
  `bbox_note(kind="followup")` naming the tool and the gap. This is the second
  half of the loop: adoption proves a tool earns its keep; a gap note explains
  *why a good-on-paper tool isn't being used*, which is the actionable signal
  for fixing bugs, expanding capabilities, and reshaping surfaces. It directly
  attacks the operator's observed failure mode — agents doing one perfunctory
  call then reverting to built-ins — by converting that silent reversion into
  feedback. The directive is cross-cutting policy, so it is appended **once at
  the engine's delivery choke point** (not repeated per rule), and it replaces
  the per-rule "if this doesn't apply, disregard" tails with a single
  not-applicable escape hatch. Gap notes land in the standard substrate-gap
  store and surface through `bbox_inbox` / `bbox_notes(kind="followup")`.

## Build order

1. **§1 system-prefix split.** Correctness fix; land independently.
   `compose_system → (static, volatile)`; transports render two segments;
   `cache_control` on the static segment only.
2. **Hook scaffold + ledger.** The three hook points, the `nudges` `side` cell
   (gating state only — fired-set + cooldowns), and the two delivery
   mechanisms — with **one** trivial rule, to prove the plumbing. Adoption is
   not instrumented in the harness; it is the transcript query of §6.
3. **Nudger v1.** The 2-3 highest-quality behavioral rules + the refactor
   signpost, harness-shipped.
4. **Catalog metadata channel** — see
   [`backlog-hooks-catalog-metadata.md`](./backlog-hooks-catalog-metadata.md).
   Only after §6 shows adoption. Atom/SM/tool `nudge_triggers` → daemon-compiled
   blob → generic harness engine.

## Non-goals

- Claude Code's user-facing hook UX (shell hooks, permission gates, user
  config). This seam is internal and has no permission semantics.
- Rejecting or removing tools. Nudges steer; they never gate. (Privilege lives
  in `SafetyPolicy` + the brofile allow/deny layer — see
  `bro-harness-tool-surface.md`.)
- Cross-session / cross-bro nudge state. The ledger is session-scoped, same as
  the clipboard and the `activated` set.
- A nudge that mutates the wire `tools` array (would defeat the §1 cache fix).

## Relationship to the other harness docs

- Transport / loop / tiering and the cache hierarchy:
  [`anthropic-harness.md`](./anthropic-harness.md).
- The built-in surface nudges steer toward (`clip_*`, `bbox_slice_*`,
  `content_search`, refactor): [`bro-harness-tool-surface.md`](./bro-harness-tool-surface.md).
- The settled-ref clipboard that the copy-paste nudge points at, and the `side`
  persistence pattern the ledger reuses:
  [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
- The ref ABI whose "observer mutates shared state across turns" pattern this
  generalizes from data to guidance:
  [`bro-harness-tool-chaining.md`](./bro-harness-tool-chaining.md).

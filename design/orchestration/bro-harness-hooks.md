---
title: "bro-harness hooks & nudges (the ambient-meta seam)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - orchestration
  - surfaces
brief: "An internal, harness-owned interception subsystem for the bro-harness agent loop. Named hook points observe loop state (user turn, assistant turn, tool result) and contribute ambient meta — the first consumer being a Nudger that surfaces blackbox atoms/tools/system-memories when behavioral or lexical triggers fire. Separates what is injected (a nudge) from where it lands (delivery mechanism), and depends on a static/volatile split of the system prompt for cache safety."
---

# bro-harness hooks & nudges (the ambient-meta seam)

> **Status.** Proposed. Verified against code 2026-05-29:
> `crates/bro-harness/src/{agent_loop.rs,registry.rs,session.rs,transport/anthropic.rs}`.
> Prerequisite §1 (system-prefix split) is a correctness fix worth landing on
> its own, independent of nudges.

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

A hook is small:

```rust
pub struct HookCtx<'a> {
    pub turn: u64,
    pub user_prompt: &'a str,
    pub assistant_text: &'a str,
    pub tool_calls: &'a [ToolCall],
    pub last_result: Option<(&'a ToolCall, &'a ToolResult)>, // on_tool_result only
    pub ledger: &'a mut NudgeLedger,                         // persisted in `side`
}

pub trait Hook: Send + Sync {
    fn evaluate(&self, cx: &mut HookCtx) -> Vec<Nudge>;
}
```

## §3 — Delivery mechanisms (the persistence trade)

A `Nudge` is delivered by one of two mechanisms, and the choice is dictated by
its lifetime — which resolves the token-accumulation worry cleanly:

| Mechanism | Where it lands | Lifetime | Use for |
|---|---|---|---|
| **Tool-result rider** | appended to an existing `tool_result`'s content (`<harness-note>…</harness-note>`) | **persists** in the transport snapshot; round-trips forever | **one-time signposts** — one line, once, adjacent to the triggering action |
| **Volatile system tail** | §1 block 2 (Anthropic system tail / OpenAI trailing developer msg) | **ephemeral** — recomposed each turn; appears while the condition holds, vanishes otherwise | **periodic / ambient reminders** — never accumulates |

So the two nudge *categories* map to two *delivery mechanisms*, and the
persistence semantics fall out for free: a periodic reminder delivered as a
rider would pile up in history; delivered as a volatile tail it is replaced each
turn. A one-time signpost delivered as a rider is contextual and then becomes
cheap history.

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
| `file_read` output substring reappears in a later `file_write`/`file_edit` | `clip_*` / `bbox_slice_*` (server-side move, no context round-trip) |
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
2. **Catalog metadata as v2.** Hardcoding regex→atom in the harness rots the
   moment the atom catalog changes and violates the project's "system memories
   are signposts, not ledgers" rule. End-state: atom/SM/tool descriptors carry
   optional `nudge_triggers` metadata; the daemon compiles the active set into a
   blob injected at dispatch (same channel discipline as `--mcp-config`). The
   harness stays a **generic** engine that knows nothing about specific atoms;
   adding an atom can ship its own nudge without touching the harness. Source of
   truth stays in `atom_search`/`atom_describe`.

Do not build the metadata channel until the adoption loop shows the engine
earns its keep.

## §5 — The nudge ledger (state)

A `nudges` cell in the session `side` blob — the plumbing already exists. Both
`Restored.side` (`session.rs:36`) and `SaveState.side` (`session.rs:46`) are
live, and `agent_loop.rs` already seeds `todos` from `side` on open and flushes
it back at save (`:66`, `:166`). The ledger rides the same path:

```text
NudgeLedger {
  fired:    Set<rule_id>,          // one-time signposts never repeat across exec→resume
  cooldown: Map<rule_id, turns_remaining>,  // periodic debounce
  log:      Vec<{rule_id, fired_turn, target}>,  // adoption instrumentation (§6)
}
```

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
- **Adoption instrumentation closes the loop.** Log `nudge_fired(rule, turn)`,
  then check whether the steered-toward tool is called within K turns. This is
  itself just another hook on the same points. It turns the operator's
  hypothesis into a falsifiable, A/B-able claim: a rule whose target is adopted
  after firing is earning its keep; a rule that gets one perfunctory call then
  fallback (or no call) is noise and gets pruned. Surfaceable via `bro_report`.

## Build order

1. **§1 system-prefix split.** Correctness fix; land independently.
   `compose_system → (static, volatile)`; transports render two segments;
   `cache_control` on the static segment only.
2. **Hook scaffold + ledger.** The three hook points, `HookCtx`, the `nudges`
   `side` cell, and the two delivery mechanisms — with **one** trivial rule and
   adoption logging, to prove the plumbing and the feedback loop.
3. **Nudger v1.** The 2-3 highest-quality behavioral rules + the refactor
   signpost, harness-shipped.
4. **Catalog metadata channel.** Only after §6 shows adoption. Atom/SM/tool
   `nudge_triggers` → daemon-compiled blob → generic harness engine.

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

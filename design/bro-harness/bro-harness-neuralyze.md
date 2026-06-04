---
title: "bro-harness neuralyze (rewind + carry a message)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "A harness-owned time-travel primitive: rewind the agent loop's context (and optionally its file mutations) to a prior checkpoint, then re-emit a single replacement message at that point. One tool, two callers — an external orchestrator steering a session via plain prose, or the agent self-correcting out of a poisoned context. Built on an auto-per-turn checkpoint substrate; detection is delegated to existing supervision telemetry, not reinvented."
---

# bro-harness neuralyze (rewind + carry a message)

> **Reframe (2026-06-04, post-harness-in-daemon — read the rest through this
> lens).** This doc predates the harness-in-daemon consolidation and the NARF
> substrate, and on re-read it splits into a sound half and a parked half along the
> exact line drawn by [`narf-effects-and-safety.md`](./narf-effects-and-safety.md):
>
> - **Context rewind is the sound, primary half — build it.** Truncating the
>   message vec to a watermark and appending one inbound `user` turn is **pure
>   conversation state**: it has *no* externality or idempotence problem, because
>   rewinding the attention window has zero world-side effect. It is cheap
>   (`tx.snapshot()`/`restore()`, already owned) and it carries the headline value
>   (rewind the *talk*, keep the *work* — an operator steering a discussion past a
>   bad instruction). v1 is the checkpoint substrate + context-only rewind + the
>   carry message + advised/self callers + guards (build steps 1–2 and 4).
> - **File revert (`keep_files=false`, the inverse-diff journal, build step 3) is
>   the parked Tx/rollback apparatus — do NOT build it.** It is a worktree
>   transaction by another name and carries the same flaws: (1) it can only invert
>   what it journaled, so externalities — an undeclared `curl`, a `git push`, a
>   spawned bro, spend, a sent message — never revert (the `shell_run` `touches`
>   snapshot covers only *declared* effects); (2) on a trusted, attended,
>   git-backed box it reimplements, worse, what git already does; (3) reverting
>   files while externalities stand makes the disk/context mismatch it is meant to
>   fix *worse*, not better. So `keep_files=true` (context-only) is the honest
>   default and only mode; file revert, if ever wanted, is git's job, not a
>   harness journal that pretends to undo the world. A NARF cell is arbitrary code
>   at shell trust — the same reason a v1 cell carries no transaction.
> - **Unify the checkpoint substrate with NARF's journal/replay, don't build a
>   second time-machine.** Neuralyze's per-turn checkpoints and NARF's
>   `resumeFromRunId` journal (cached settled calls) are the *same shape* — watermark
>   the loop, truncate to a point, replay or re-emit. They should share one
>   substrate. And NARF replay is the **sound answer to the idempotence flaw** the
>   inverse-diff journal lacks: it caches settled results and never re-executes them,
>   sidestepping "re-run an external effect" by not re-running at all.
>
> Net: neuralyze v1 = checkpoint substrate + context-only rewind + carry +
> callers/guards, sharing NARF's replay substrate; the file inverse-diff journal is
> dropped. Sections below describing `keep_files=false` and the inverse-diff journal
> are retained as the parked design, not v1 scope.

> **Status.** Proposed. Verified against code 2026-05-29:
> `crates/bro-harness/src/{agent_loop.rs,session.rs,transport/anthropic.rs}`,
> `src/orchestration/supervision.rs`. Depends on the file inverse-diff journal
> sketched in the checkpoint discussion (not yet built) and reuses the `side`
> persistence spine already in the loop.

## Problem

RLHF-trained models have a well-known failure mode: once a context starts to
**flail** — a tool error spiral, a cancellation cascade, a wrong design premise
the conversation then builds on — the poisoned tokens stay in the attention
window and every subsequent turn conditions on them. The model attends to its
own thrash; "be helpful / recover gracefully" pressure generates *more* output
conditioned on the bad context, deepening the spiral. (Observed live in this
project's own sessions: parallel-bash cancellation cascades that fed the next
batch.)

The only remedy available today is the blunt one: **discard the session, re-feed
the docs, start fresh.** That throws out the *diagnosis along with the disease* —
the fresh agent doesn't know the rake is there and steps on it again. It also
throws out all real progress.

The same shape appears in **design conversations** with no tooling at all: an
operator gives a muddy instruction, the discussion goes off-track, and several
turns of reasoning are now built on the bad premise. The clean move — "rewind to
before my bad instruction and re-emit a refined one" — has no mechanism.

These are one problem: **the valuable thing a poisoned timeline produced is a
lesson (or a corrected premise); the only tool we have to drop the poison also
drops the lesson.**

## The idea

Decouple the lesson from its toxic substrate. Rewind the timeline to a chosen
point, discard everything after it, and carry **one** distilled message back to
that point. Compared to the alternatives:

| strategy | keeps progress | keeps lesson | drops poison |
|---|---|---|---|
| push through (status quo) | ✓ | ✓ | ✗ — the killer |
| full reset | ✗ | ✗ | ✓ |
| **neuralyze** | ✓ (via kept files) | ✓ (the message) | ✓ |

Neuralyze is the only strategy that hits all three. It is named for the
time-travel framing: rewind, but carry a message to your past self.

> This is a **conversation instrument first.** The premier use case is an
> operator surgically rewinding a *discussion* past a bad instruction and
> re-emitting a refined one — a case with zero tool calls. Flail-recovery is the
> instance where the agent points the same instrument at itself.

## The primitive

**One new tool.** No changes to `bro_resume`; no dual API.

```
neuralyze(to, message, keep_files = false)
  → drop the target checkpoint and everything after it
  → revert files to the target's journal watermark (unless keep_files)
  → append `message` as the next inbound (user-role) turn
  → continue the loop
```

`to` resolves to a checkpoint (see substrate). `message` is the time-travel
message `M`. `keep_files=true` rewinds conversation only, leaving file mutations
in place.

### `message` is always an inbound (user-role) turn

`M` is never authored by the model — it is authored by the operator (advised) or
is a note *to* past-self (self). A message the model must *read and condition
on* cannot be an `assistant` turn. So `M` always arrives as a `user`-role turn,
tagged as a harness/temporal note when self-authored. There is nothing to infer
from the target's role.

What *survives above* the target is what makes `M` read differently to a human:

- **Target is a user instruction** (cp-K is a user turn): that instruction is
  dropped; `M` takes its seat and *is* the new instruction. ← operator steering.
- **Target is an assistant decision** (cp-K is an assistant turn): the decision
  and its fallout are dropped, but the **user instruction above it survives** (we
  drop *from* cp-K, not before it); `M` arrives as a fresh note on top of the
  intact task. ← flail recovery: keep the task, add the warning.

Mechanically identical: `M` is always just the next inbound user turn. The
operator's own "neuralyze to cp-K, message=…" prose lives *after* cp-K, so it is
truncated away by the rewind — only the distilled `M` survives. **The instruction
to rewind erases itself along with the mistake.**

## Checkpoint substrate

### Auto, every turn, foresight-free

The neuralyzer exists to escape spirals you did not see coming, so explicit
"mark a checkpoint here" **cannot** be the substrate — by definition you didn't
know to mark it. Every loop turn (user and assistant alike) is implicitly
checkpointed. This is cheap: a checkpoint is a **watermark**, not a copy —

```
cp-7 → { msg_prefix_len, file_journal_watermark, role, summary, label? }
```

`summary` is harness-derived (first line of the turn's text + its tool-call
names) so both the stream stamp and the list are scannable with zero model
effort. A checkpoint costs an integer pair + a short string.

### Monotonic, never-reused ids

The counter only increases and ids are never recycled. Rewinding to `cp-5`
truncates `cp-6`/`cp-7` out of the live set; the next generated checkpoint is
`cp-8`, not a reused `cp-6`. Consequences:

- A given id denotes one specific historical event **for all time** — an advisor
  that saw `cp-7` in the stream can never have it silently re-alias.
- Targeting an id in an abandoned branch **errors** (`cp-7 is in an abandoned
  branch`) rather than hitting the wrong turn.
- Gaps in the live set are expected and honest.

### One id space, two discovery paths

The id is the lingua franca; how each party *finds* the right id differs by
vantage point (this is the core constraint — the scheme must be unambiguous to
both an external orchestrator and the self-reflecting agent):

- **External orchestrator** thinks positionally ("the turn it started the
  parallel batch"). Every assistant-turn event in the stream-json envelope is
  **stamped** `checkpoint: "cp-K"`. The advisor watching the stream reads the id
  directly off the action it wants to undo.
- **Self / agent** thinks semantically ("before I went down that path") and does
  not reliably know its own turn numbers. A read tool **`list_checkpoints`**
  (shape mirrors `shell_list`) returns `[{id, role, summary, label?}]`; the agent
  scans past-turn summaries and recognizes the fork.

### Targeting forms

`to` accepts, resolved in this order:

1. **symbolic anchor** — `last-user` (the boundary of the last user/instruction
   turn: "scrap everything since the last instruction"). The natural self-target
   for a flail.
2. **label** — an optional semantic alias (below).
3. **literal cp-id** — `cp-7`.

**No relative addressing** (`back: 4`). It is TOCTOU-fragile: the advisor counts
"4 back" from what it *observed*, but turns may have advanced between observation
and the prose landing, so the offset drifts. Absolute ids and symbolic anchors
do not drift. (Same rationale as the standing preference for stable handles over
positional ones.)

`to` is effectively required — there is no fuzzy default — but `last-user` is the
easy answer when the caller does not want to inspect a list.

### Optional labels (foresight overlay)

A tiny `checkpoint(label="pre-migration-edit")` tags the *current* turn; the
label aliases that turn's cp-id. This is sugar for the case where foresight
*did* exist ("about to do something I may want to cleanly undo"), and it gives an
advisor a semantic handle to speak. The auto substrate guarantees targeting
always works; labels only make some targets nameable.

## Detection is delegated

Spiral detection is **not** the harness's job — **`src/orchestration/supervision.rs`**
already implements task-local mechanical telemetry with exactly the needed
signal:

| AlertKind | Amber | Red |
|---|---|---|
| **loop** (consecutive identical tool/input hash) | 3 | 6 |
| compaction (markers / 300s) | 2 | 4 |
| token_burn (ratio vs baseline) | 2.0× | 3.0× |

Idle since last event is no longer an alert — it is a neutral `idle_seconds` /
`idle_notice` fact surfaced past a single configurable threshold
(`stall_notice_ms`, default 180s), with no severity.

Crucially, supervision telemetry is deliberately **detect-and-surface only** — it
"does not cancel, steer, or choose recovery" (that is documented non-scope; those
belong to the advisor layer above it). That split maps onto neuralyze exactly:

1. **Detection** = `supervision::AlertKind::loop` (exists; already in task status).
2. **Decision** = the advisor/orchestrator reads the alert (its designed job) and
   sends prose: *"call neuralyze, to=cp-7, message=…"*.
3. **Action** = the `neuralyze` tool (the only new thing).

No new detection machinery in the harness.

## Two callers, one tool

The "two use cases" are not two code paths — they are two **sources of the same
tool call**, differing only in who authored `message`:

- **Advised (default, safe).** The arbiter is *outside* the poisoned context: an
  operator or advisor watching via stream / `bro_status`. It authors `M` from a
  sober vantage and delivers it as **plain prose through the normal `bro_resume`
  channel** — "call neuralyze, to=cp-7, message='go serial when probing
  unknowns'." The agent is a **mechanical relay**: see clear imperative → call
  tool with given args (the lowest-cognitive-load action a shaky agent can take).
  This is why no `bro_resume` changes are needed — the prose vector is free.
- **Self (experimental, gated).** The agent calls `neuralyze` on its own
  judgment. Riskier — the carry is authored from inside the context it is trying
  to escape — so it is gated by conservative tooldoc language ("only after
  repeated failed attempts at the same operation, with a specific actionable
  lesson — not for ordinary retries") and **instrumented** (below).

Same tool; `message` does not care who wrote it. "Advised vs self" is purely a
fact about the string's origin, invisible to the harness.

### Authority resolves the earlier open question

The earlier checkpoint-authority fork (self-service vs orchestrator-held) is not
either/or — it is these two entry points over one shared substrate. Both ride the
same checkpoint journal and the same context-truncation; they differ only in who
decides and who writes `M`.

## Loop guards (self-invocation)

The dangerous shape is self-neuralyze looping — same wall, rewind, same wall.

- **Budget.** A small cap (e.g. 2–3) of self-neuralyzes per task. Exhausted →
  escalate (surface to the orchestrator), do not loop. Tracked in the `side`
  cell.
- **Carries accumulate visibly.** A second rewind to the same vicinity shows the
  prior carry, so repetition is itself signal (and argues for a low budget — the
  carry re-introduces a little poison each time).
- **Behavior must change.** A rewind whose `M` does not name a *different* action
  is budget-burning; near-identical successive carries should force escalation
  rather than another attempt.
- **Don't rewind past comprehension.** Rewinding too far lands `M` before the
  context in which it is meaningful ("watch out for foo" before foo exists). The
  natural target is *just before the decision that went wrong* — which is why
  per-turn cp-ids and `last-user` exist rather than blunt session-start.

Advised neuralyze is not budgeted by the harness (the external arbiter owns that
judgment), but the same instrumentation applies.

## Instrumentation (earns-its-keep loop)

Every neuralyze logs `{source: self|advised, to, depth, message, keep_files}` and
— the load-bearing metric — **whether the post-rewind timeline succeeded where
the prior one failed.** A rewind that does not change the outcome is burning
budget, and it shows in the data. Same falsifiable-on-evidence discipline as the
hooks-doc Nudger adoption log. Surfaceable via `bro_report`.

## Scope: context vs files

`keep_files` (default false):

- **false** — revert both conversation and file mutations to the target
  watermark. The coherent default for a headless agent: don't hand past-self a
  context that mismatches disk.
- **true** — rewind conversation only. The legitimate exception the premier case
  exposes: *"the code you wrote is fine, but this design tangent is wrong —
  rewind the discussion, keep the files."* For pure design discussion there are
  no journaled mutations, so both modes coincide; the param only bites when files
  changed.

File revert requires the **inverse-diff journal** (record each mutating tool
call's pre-image; `revert_to(watermark)` replays inverses). The journal is
**harness-local**: pre-images are captured at mutation time inside the harness's
own `file_write` / `file_edit` / `shell_run` tools (`bro-tools` `workspace.rs` /
`shell.rs`), which already hold the original bytes, and `shell_run`'s
declared-`touches` snapshot covers shell side effects (the gap Claude Code's
rewind documents as unsupported). The daemon's `refactor::apply` /
`SliceApplyOptions` plane (`src/refactor/**`, `src/slices.rs`) captures
`original_sha256` + bytes for its own apply/rollback — but that is **prior art for
the shape, not a runtime dependency**: neuralyze must not call into daemon
`refactor` code (DX-9 / the no-runtime-daemon-dependency convention). If that
snapshot primitive is worth sharing, it is extracted into a crate both link, never
reached as a service. Context rewind alone is trivial — the harness owns the
message vec (`tx.snapshot()`/`restore()`); the journal is the one substantial
build.

## Persistence

Checkpoints (`cp-id → watermark/role/summary/label`), the file inverse-diff
journal, and the self-neuralyze budget all ride the **`side` cell** — the spine
already in the loop (`agent_loop.rs` seeds `todos` from `side` on open, flushes
at save; `SaveState.side` / `Restored.side` are live). So neuralyze survives
`exec → resume`: an advisor can rewind a session it resumed days later,
addressing a cp-id it recorded then. No new persistence machinery.

## Relationship to the hooks doc (the escalation ladder)

Neuralyze is the **top rung** of one steering system whose lower rungs are the
[`bro-harness-hooks.md`](./bro-harness-hooks.md) nudges. By intervention strength:

| rung | intervention | state touched | poison handling | author |
|---|---|---|---|---|
| 1 | nudge (hooks doc) | none | leaves it, adds counter-signal | harness hook |
| 2 | steer-on-resume (prose) | +1 turn | leaves it, adds correction | external |
| 3 | **advised neuralyze** | context (+files) rewound | **drops it**, carry survives | external |
| 4 | **self neuralyze** | context (+files) rewound | drops it; carry authored from inside | agent |

Detection feeding rungs 3–4 is `supervision::AlertKind::loop`, surfaced to the
arbiter; the lower rungs are the hooks subsystem's `on_tool_result` nudges. One
system, gentle-to-heavy.

## UI surfacing (forward-looking)

There is no TUI today. Keep cp-id surfacing **presentation-agnostic**: ids live
in the stream-json envelope (per-turn stamp) and in `list_checkpoints`. A future
TUI then renders existing ids (e.g. a rewind affordance per turn) without any new
core mechanism — the same way Claude Code's `/rewind` is a UI over checkpoints,
except here the substrate is also addressable by the agent and external callers.

## Build order

1. **Checkpoint substrate.** Auto-per-turn cp-ids in `side`; stream-json
   per-turn stamp; `list_checkpoints` (last-N default, `all` option).
2. **Context-only neuralyze.** `neuralyze(to, message, keep_files=true-equiv)` —
   truncate message vec to watermark, append `M`. This alone delivers the
   premier conversation-steering case (design discussions have no file journal).
3. **File inverse-diff journal + `keep_files=false`.** The substantial build;
   reuse `refactor::apply` snapshot machinery; extend to `shell_run` `touches`.
4. **Self-invocation + guards + instrumentation.** Tooldoc-gated; budget,
   accumulating carries, behavior-change check, earns-its-keep log.

Detection (supervision `loop` alert → advisor) needs no new build; it is wiring
an existing signal to the advisor's existing decision role.

## Non-goals

- **Forward / redo.** Strictly backward; abandoned branches stay dead. Redo is a
  branch model whose value is marginal.
- **Reinventing detection.** `supervision.rs` owns mechanical anomaly detection;
  neuralyze consumes its `loop` alert, it does not re-derive it.
- **`bro_resume` API changes.** The advised path is plain prose through the
  existing channel. (A direct external override that truncates a session whose
  agent is too far gone to relay a tool call is a possible v2 backstop for the
  deep-spiral case — logged, not built.)
- **Fuzzy/semantic target resolution.** Targets are absolute cp-ids, labels, or
  the `last-user` anchor. No "rewind to roughly where I mentioned X."
- **Cross-session / cross-bro checkpoint sharing.** Checkpoints are
  session-scoped, like the clipboard, todos, and the `activated` set.

## Relationship to the other harness docs

- Detection substrate: `src/orchestration/supervision.rs` (mechanical telemetry,
  `AlertKind::loop`) and the advisor layer in
  `design/orchestration/supervision/supervision.md`.
- The lower rungs of the steering ladder and the `on_tool_result` seam:
  [`bro-harness-hooks.md`](./bro-harness-hooks.md).
- The `side` persistence spine the checkpoint/journal/budget cells reuse:
  [`bro-harness-clipboard.md`](./bro-harness-clipboard.md).
- Transport / loop / `snapshot`/`restore` that context-rewind drives:
  [`anthropic-harness.md`](./anthropic-harness.md).

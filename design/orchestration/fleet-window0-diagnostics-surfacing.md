---
title: "Fleet TUI: surfacing window-0 diagnostics (observing vs ignoring)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - orchestration
  - surfaces
brief: "Make window-0 diagnostics riders visible in `bro fleet` and turn them into an at-a-glance signal of whether an agent is acting on vs ignoring the diagnostics its own edits produce. Phase 1 (shipped) renders the rider distinctly in the single-agent transcript view. Phases 2-3 (proposed) fold the rider's new/fixed counts into a per-agent outstanding-errors badge on the roster, and derive the unused `FleetState::Alerting` when outstanding errors persist across turns - the actual acting-vs-ignoring telemetry. The rider counts already ride the transcript, so 2-3 are TUI-side folds, not harness changes."
---

# Fleet TUI: surfacing window-0 diagnostics (observing vs ignoring)

> **Status.** Partial. Phase 1 shipped (`feat(fleet): surface window-0
> diagnostics riders distinctly in the TUI`). Phases 2-3 proposed. Grounded
> against code 2026-05-30: `src/fleet_tui.rs`, `src/orchestration/fleet.rs`
> (`parse_transcript`/`parse_user_event`/`TranscriptItem`/`TaskSnapshot`),
> `crates/bro-harness/src/diagnostics/render.rs` (`RIDER_MARKER`,
> `build_rider`), `crates/bro-harness/src/agent_loop.rs` (the seam).

## Problem

Window-0 diagnostics ([bro-harness-diagnostics](bro-harness-diagnostics.md))
append a rider to the tool result of each Rust file edit — the agent sees what
its edit produced before acting again. But the **operator** running a fleet has
no view of this: are the agents *seeing* their diagnostics and fixing them, or
building further on broken foundations? That second case — ignoring the channel
— is the exact failure window-0 exists to prevent, and at the fleet level it is
invisible.

The goal: surface riders in `bro fleet`, and make "is this drone acting on its
diagnostics or ignoring them?" answerable at a glance, without reading
transcripts.

## What the rider already carries

`render::build_rider` emits a block opened by a stable marker
(`RIDER_MARKER = "window-0 diagnostics:"`) with a counts summary and per-file
detail:

```
window-0 diagnostics: 2 new (1 error), 1 fixed, 5 carried
  src/lib.rs:
    12: cannot find type `Bar` in this scope
```

- **new** — diagnostics this edit introduced (or changed) vs the file baseline.
- **fixed** — baseline diagnostics this edit removed.
- **carried** — pre-existing diagnostics left unchanged (not the agent's doing).

The harness appends this (after a `\n\n` separator) to the `file_edit` /
`file_write` tool-result content, which flows verbatim into the fleet TUI's
single-agent transcript (`parse_user_event` → `TranscriptItem::ToolResult`).
**The signal is already in the transcript** — Phases 2-3 are folds over it, not
new harness or daemon plumbing.

## Phase 1 — distinct rider rendering (SHIPPED)

The rider reached the single-agent transcript view already, but concatenated
onto the tool's JSON body, undifferentiated and subject to tool-verbosity
gating. Phase 1 pulls it out and renders it as its own always-visible block:

- **`render.rs`** — `RIDER_MARKER` constant; the seam (`append_window0_diagnostics`)
  `\n\n`-separates the rider from the tool body.
- **`fleet.rs`** — `TranscriptItem::ToolResult` carries `rider: Option<String>`;
  `split_window0_rider` peels the marker-delimited block off at parse time. The
  marker string is duplicated in `fleet.rs` with a wire-contract comment because
  `blackbox` and `bro-harness` are sibling crates that do not share types.
- **`fleet_tui.rs`** — the rider renders independent of tool-verbosity gating:
  summary line `⚠`-flagged bold-yellow, detail lines yellow, directly under the
  edit that produced it.

This delivers visibility. It does **not** answer acting-vs-ignoring at the fleet
level — for that you still read the transcript. That is Phases 2-3.

## Phase 2 — roster outstanding-errors badge (PROPOSED)

Fold the rider counts per agent into a single **outstanding window-0 errors**
number and show it on the roster row.

- **The fold:** walk an agent's transcript riders in order; each rider's `new`
  errors add, each `fixed` subtracts. The running total is "errors this agent's
  edits introduced and has not yet resolved." (Use the *error* subset of `new`,
  not warnings/lints; `carried` is excluded — it is not the agent's debt.)
- **Plumbing (the gap):** the roster renders from `TaskSnapshot` → `AgentView`,
  which today carries no tool-result content (only status/counts/snippets). So
  compute the fold in `parse_transcript` (where the riders are already parsed)
  and bubble a derived **count** — e.g. `TaskSnapshot.window0_outstanding: usize`
  (plus maybe `window0_total_new` / `window0_fixed` for context) — into
  `AgentView`. This is a small derived integer, not the raw rider body, so it is
  cheap to carry on the snapshot.
- **Render:** a `⚠N` badge in the roster row (a new column, or a suffix on the
  agent name). Zero outstanding → no badge.

## Phase 3 — `Alerting` derivation: the acting-vs-ignoring signal (PROPOSED)

The outstanding count *trending* is the real signal:

- trending **down** → the agent is seeing and fixing what its edits broke
  (observing / acting).
- **stuck or growing** across turns while the agent keeps doing other work → it
  is building on broken foundations (ignoring).

So: when outstanding errors **persist across K turns/checks without dropping**,
derive `FleetState::Alerting` — the variant that already exists in `fleet_tui.rs`
but is currently never set (see the stub comment ~line 57-58). That promotes the
agent's roster glyph to a red `!`, turning "is this drone ignoring its
diagnostics?" into a glance.

`K` is the tuning knob (how many turns of persistence before it counts as
ignoring) — see open questions.

## Design notes

- **No harness/daemon change needed for 2-3.** The rider counts are already in
  the transcript stream the fleet TUI parses; 2-3 are TUI-side folds plus the
  `TaskSnapshot`→`AgentView` field for the derived count.
- **`RIDER_MARKER` is the coupling point.** The fold depends on parsing the
  rider's count line; keep the marker + summary format (`N new (E error[s]),
  F fixed, C carried`) stable, or version it, since `fleet.rs` parses it.
- **Outstanding ≠ carried.** The actionable signal is `new` errors the agent has
  not driven to `fixed`; `carried` is pre-existing debt the edit did not touch
  and must not inflate the badge.
- **Errors first; lints later.** MVP window-0 is error-tier only. When the
  check/lint tier lands (deferred — see bro-harness-diagnostics), decide whether
  the badge counts lints too or stays errors-only.

## Open questions

- **`K` (persistence threshold).** Turns? consecutive window-0 checks? wall time?
  Too low → false "ignoring" on an agent mid-multi-step-fix; too high → misses
  real ignore-spirals. Likely turns-since-outstanding-last-dropped, small (2-3).
- **Cross-file resolution.** An agent may fix an error by editing a *different*
  file (e.g. add the missing `type Bar`); the per-file diff may not credit the
  fix cleanly. Does the fold need cross-file awareness, or is the per-file
  `fixed` count good enough in practice?
- **Decay / reset.** When an agent legitimately abandons a file or the task
  scope shifts, stale outstanding counts should decay rather than pin the agent
  to `Alerting` forever.
- **Granularity.** Per-agent badge (simple) vs per-file breakdown in the detail
  view (richer). Start per-agent.
- **Acted-on-last-rider marker.** A lighter per-turn signal ("did the turn after
  this rider reduce its errors?") might be enough without the full fold — worth
  prototyping alongside Phase 2.

## Why shelved

Phase 1 delivers the direct ask (riders are visible). Phases 2-3 deliver the
at-a-glance acting-vs-ignoring signal but need the roster plumbing and a tuning
threshold whose right value is unknown until watched in practice. Deferred until
"is this drone ignoring its diagnostics?" becomes a real chore at the fleet
level. **Revisit trigger:** running multi-agent fleets where reading each
transcript to check diagnostics adoption is the bottleneck.

---
title: "Fleet TUI: surfacing window-0 diagnostics (observing vs ignoring)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "Make window-0 diagnostics riders visible in `bro fleet` and turn them into an at-a-glance signal of whether an agent is acting on vs ignoring the diagnostics its own edits produce. Phase 1 (shipped) renders the rider distinctly in the single-agent transcript view. Phases 2-3 (proposed) fold the rider's new/fixed counts into a per-agent outstanding-errors badge on the roster, and derive the unused `FleetState::Alerting` when outstanding errors persist across turns - the actual acting-vs-ignoring telemetry. The rider counts already ride the transcript, so 2-3 are TUI-side folds, not harness changes."
---

# Fleet TUI: surfacing window-0 diagnostics (observing vs ignoring)

> **As-built record.** Phase 1 shipped (`feat(fleet): surface window-0
> diagnostics riders distinctly in the TUI`). Phases 2-3 (roster
> outstanding-errors badge + `Alerting` derivation) were excised to
> [`backlog-window0-roster-alerting.md`](./backlog-window0-roster-alerting.md) —
> shelved until reading each transcript to check diagnostics adoption becomes the
> fleet-level bottleneck. Grounded against code 2026-05-30: `src/fleet_tui.rs`,
> `src/orchestration/fleet.rs`
> (`parse_transcript`/`parse_user_event`/`TranscriptItem`/`TaskSnapshot`),
> `crates/bro-harness/src/diagnostics/render.rs` (`RIDER_MARKER`,
> `build_rider`), `crates/bro-harness/src/agent_loop.rs` (the seam).

## Problem

Window-0 diagnostics ([bro-harness-diagnostics](../bro-harness/bro-harness-diagnostics.md))
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

## Phases 2-3 (proposed — extracted)

The roster outstanding-errors badge (Phase 2) and the `FleetState::Alerting`
acting-vs-ignoring derivation (Phase 3) — both TUI-side folds over the rider
counts already in the transcript, plus the design notes and open questions (`K`
threshold, cross-file resolution, decay/reset, granularity) — were excised to
[`backlog-window0-roster-alerting.md`](./backlog-window0-roster-alerting.md).

## Why shelved

Phase 1 delivers the direct ask (riders are visible). Phases 2-3 deliver the
at-a-glance acting-vs-ignoring signal but need the roster plumbing and a tuning
threshold whose right value is unknown until watched in practice. Deferred until
"is this drone ignoring its diagnostics?" becomes a real chore at the fleet
level. **Revisit trigger:** running multi-agent fleets where reading each
transcript to check diagnostics adoption is the bottleneck.

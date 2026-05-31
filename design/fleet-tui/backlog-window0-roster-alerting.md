---
title: "Fleet TUI — window-0 roster badge + Alerting derivation (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "Turn the already-visible window-0 diagnostic riders into an at-a-glance acting-vs-ignoring signal. Phase 2 folds each agent's rider new/fixed counts into a per-agent outstanding-errors badge on the roster; Phase 3 derives the unused FleetState::Alerting when outstanding errors persist across K turns without dropping. Both are TUI-side folds over the transcript stream — no harness or daemon change. Shelved until reading transcripts to check diagnostics adoption becomes the fleet-level bottleneck."
---

# Fleet TUI — window-0 roster badge + Alerting derivation (backlog)

> **Provenance.** Extracted from
> [`fleet-window0-diagnostics-surfacing.md`](./fleet-window0-diagnostics-surfacing.md)
> (Phase 1 — distinct rider rendering — is shipped). The rider counts already
> ride the transcript, so both phases are TUI-side folds, not new harness/daemon
> plumbing.

## Status / gate

**Shelved, not blocked.** Phase 1 delivers the direct ask (riders are visible).
Phases 2-3 deliver the at-a-glance acting-vs-ignoring signal but need the roster
plumbing and a tuning threshold whose right value is unknown until watched in
practice. **Revisit trigger:** running multi-agent fleets where reading each
transcript to check diagnostics adoption is the bottleneck.

## Phase 2 — roster outstanding-errors badge

Fold the rider counts per agent into a single **outstanding window-0 errors**
number and show it on the roster row.

- **The fold:** walk an agent's transcript riders in order; each rider's `new`
  errors add, each `fixed` subtracts. The running total is "errors this agent's
  edits introduced and has not yet resolved." (Use the *error* subset of `new`,
  not warnings/lints; `carried` is excluded — it is not the agent's debt.)
- **Plumbing (the gap):** the roster renders from `TaskSnapshot` → `AgentView`,
  which today carries no tool-result content (only status/counts/snippets).
  Compute the fold in `parse_transcript` (where the riders are already parsed) and
  bubble a derived **count** — e.g. `TaskSnapshot.window0_outstanding: usize`
  (plus maybe `window0_total_new` / `window0_fixed` for context) — into
  `AgentView`. A small derived integer, not the raw rider body, so it is cheap to
  carry on the snapshot.
- **Render:** a `⚠N` badge in the roster row (a new column, or a suffix on the
  agent name). Zero outstanding → no badge.

## Phase 3 — `Alerting` derivation: the acting-vs-ignoring signal

The outstanding count *trending* is the real signal:

- trending **down** → the agent is seeing and fixing what its edits broke
  (observing / acting).
- **stuck or growing** across turns while the agent keeps doing other work → it
  is building on broken foundations (ignoring).

When outstanding errors **persist across K turns/checks without dropping**, derive
`FleetState::Alerting` — the variant that already exists in `fleet_tui.rs` but is
currently never set (stub comment ~line 57-58). That promotes the agent's roster
glyph to a red `!`, turning "is this drone ignoring its diagnostics?" into a
glance. `K` is the tuning knob (see open questions).

## Design notes

- **No harness/daemon change needed.** The rider counts are already in the
  transcript stream the fleet TUI parses; both phases are TUI-side folds plus the
  `TaskSnapshot`→`AgentView` field for the derived count.
- **`RIDER_MARKER` is the coupling point.** The fold depends on parsing the
  rider's count line; keep the marker + summary format (`N new (E error[s]),
  F fixed, C carried`) stable, or version it, since `fleet.rs` parses it.
- **Outstanding ≠ carried.** The actionable signal is `new` errors the agent has
  not driven to `fixed`; `carried` is pre-existing debt the edit did not touch and
  must not inflate the badge.
- **Errors first; lints later.** MVP window-0 is error-tier only. When the
  check/lint tier lands (deferred — see
  [`../bro-harness/backlog-diagnostics-truth-tiers.md`](../bro-harness/backlog-diagnostics-truth-tiers.md)),
  decide whether the badge counts lints too or stays errors-only.

## Open questions

- **`K` (persistence threshold).** Turns? consecutive window-0 checks? wall time?
  Too low → false "ignoring" on an agent mid-multi-step-fix; too high → misses
  real ignore-spirals. Likely turns-since-outstanding-last-dropped, small (2-3).
- **Cross-file resolution.** An agent may fix an error by editing a *different*
  file (e.g. add the missing `type Bar`); the per-file diff may not credit the fix
  cleanly. Does the fold need cross-file awareness, or is the per-file `fixed`
  count good enough in practice?
- **Decay / reset.** When an agent legitimately abandons a file or the task scope
  shifts, stale outstanding counts should decay rather than pin the agent to
  `Alerting` forever.
- **Granularity.** Per-agent badge (simple) vs per-file breakdown in the detail
  view (richer). Start per-agent.
- **Acted-on-last-rider marker.** A lighter per-turn signal ("did the turn after
  this rider reduce its errors?") might be enough without the full fold — worth
  prototyping alongside Phase 2.

## Relationship

- Parent / Phase 1 as-built: [`fleet-window0-diagnostics-surfacing.md`](./fleet-window0-diagnostics-surfacing.md).
- The window-0 diagnostics engine: [`../bro-harness/bro-harness-diagnostics.md`](../bro-harness/bro-harness-diagnostics.md).
- Cluster hub: [`fleet-tui.md`](./fleet-tui.md).

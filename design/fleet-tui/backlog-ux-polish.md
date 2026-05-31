---
title: "Fleet TUI — builtin-tool rendering & roster UX polish (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "Operator-feedback UX polish for bro fleet, surfaced while driving the classifier-intern vehicle on feat/fleet-classifier-intern. Three independent items: (1) a live activity throbber for the executor AND its hidden classifier/intern companion; (2) drop the roster cost column in favor of a latest-report-tool teaser per agent; (3) compact builtin tool-call rendering to a single line tool(arg1, arg2) instead of a multi-line JSON args block."
---

# Fleet TUI — builtin-tool rendering & roster UX polish (backlog)

> **Provenance.** Net-new from **thread-068f07b4** ("fleet-tui-ux-polish",
> active). Code-grounded against `src/fleet_tui.rs` 2026-05-31 — all three items
> are confirmed unbuilt. The cockpit as-built record is
> [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md).

Three independent, pickup-able items. None block each other.

## 1. Activity throbber (executor + classifier companion)

The TUI needs a live "working" indicator for **both** the executor **and** its
classifier/intern companion, so the operator sees at a glance when each is in a
turn vs idle/waiting. Long autonomous turns currently look indistinguishable from
a hung/idle session.

- **Today:** turn-in-flight is derivable (`turn_active` in `TaskSnapshot`;
  `TaskStatus::Running if snap.turn_active => FleetState::Active` at
  `fleet_tui.rs:139`) but surfaced only as a **static** glyph
  (`FleetState::Active => ("✽", Color::Cyan)`, `fleet_tui.rs:95`) — no motion.
- **Want:** an animated spinner/throbber on the roster row (and the single-agent
  header) for any agent whose turn is active. The frame advances on a tick, so it
  reads as motion, not a static character.
- **Companion visibility (the harder half):** the hidden classifier/intern
  companion has **no roster presence** today. Its activity must be visible too —
  either a second throbber on the executor's row (a sub-indicator) or a dedicated
  companion affordance. Requires surfacing the companion's turn-active state into
  the snapshot the roster reads.
- **Acceptance:** an agent mid-turn shows visible motion; an idle/waiting agent
  does not; the companion's active turn is distinguishable from the executor's.

## 2. Roster: drop the cost column → `report`-tool teaser

Cost is low-signal for live driving ("cost theatre"); the `report` teaser is what
the operator actually wants to scan.

- **Today:** the roster table carries a dedicated cost column —
  `Length(8)` constraint (`fleet_tui.rs:1075`), `"cost"` header
  (`fleet_tui.rs:1085`), `format!("${c:.4}")` cell (`fleet_tui.rs:1146`). The
  latest `report` message is rendered in the single-agent transcript
  (`fleet_tui.rs:1385`), **not** on the roster.
- **Want:** replace the cost column with the latest builtin `report` message per
  agent — a one-line status of what it's doing / needs. Reclaim the width for the
  teaser. (Roster columns today: glyph · prov · name · model · cost · turns ·
  started · last — `draw_roster`.)
- **Acceptance:** each roster row shows its agent's most recent `report` one-liner
  (truncated to fit) in place of the `$` figure; no cost column.

## 3. Compact single-line builtin tool-call rendering

Compact tool calls in the verbose transcript to a single-line `tool(arg1, arg2)`
form — positional args, no arg-name labels except where the value is non-obvious
(then `key=value`).

- **Today:** the `ToolCall` arm prints `⏺ {name}` then a multi-line
  pretty-printed JSON args block via `monospace_block(args, ARG_MAX_LINES, …)`
  (`fleet_tui.rs:1340-1347`), which is noisy. (This matches the *current* §5.4
  spec, which described a raw monospace block — so this item supersedes that
  spec.)
- **Want:** e.g. `⏺ smart_read(src/knowledge.rs)` / `⏺ shell_run("cargo test
  --lib")` on one line. Positional args by default; `key=value` only where the
  value would be ambiguous without its name. Fall back to the block form (or a
  truncation rider) when args are too large for one line.
- **Acceptance:** common tool calls render on one line with positional args;
  oversized/ambiguous cases degrade gracefully rather than dumping the full JSON.

## Relationship

- As-built cockpit (incl. the current §5.4 rendering this supersedes):
  [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md).
- Cluster hub: [`fleet-tui.md`](./fleet-tui.md).

---
title: "Fleet TUI — builtin-tool rendering & roster UX polish"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "As-built record for operator-feedback UX polish in bro fleet: focused single-agent activity strip for executor + hidden classifier/intern companion, roster cost-column replacement with the latest report teaser, and compact builtin tool-call rendering as one-line tool(arg1, arg2) forms with graceful fallback."
---

# Fleet TUI — builtin-tool rendering & roster UX polish

> **Provenance.** Net-new from **thread-068f07b4** ("fleet-tui-ux-polish",
> active). Originally recorded as backlog; re-grounded against
> `src/fleet_tui.rs` on 2026-06-02 and promoted to an as-built record.
> The cockpit as-built record is [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md).

## Status

**Landed.** The three polish items are implemented in `src/fleet_tui.rs`.
The activity implementation differs slightly from the original backlog wording:
executor/classifier motion is surfaced in the focused single-agent activity
strip rather than as a second animated cell on every roster row. The roster row
still uses the state glyph; the selected agent's header/chrome carries the
animated working/idle/waiting signal.

## 1. Activity throbber (executor + classifier companion)

The TUI has a live working indicator for **both** the executor **and** its
classifier/intern companion in the focused single-agent view.

- `App` tracks `activity_clocks` and `activity_frame`; the event loop advances
  `activity_frame` on ticks.
- `selected_activity_spans` renders the focused agent's activity segment and, if
  present, the hidden classifier companion's segment.
- `activity_segment` emits working/waiting/interrupted/finished/idle text, with
  role-specific spinner frames from `activity_spinner`:
  `Agent` uses `✽/✣/✢/✣`; `Classifier` uses `✻/✶/✷/✶`.
- `activity_clock_records_last_completed_duration` covers the active→idle clock
  transition.

**Current behavior:** an active focused executor shows animated working motion;
an active classifier companion is distinguishable by its separate magenta
Classifier activity segment. Idle/waiting states render without motion. The
roster row's leading state glyph remains static.

## 2. Roster: drop the cost column → `report`-tool teaser

Cost was low-signal for live driving ("cost theatre"); the roster now shows the
agent's latest `report` message instead.

- `AgentView` carries `report_message` from `TaskSnapshot`.
- `draw_roster` defines columns as glyph · provider · agent · model · `report`
  · started · last, with the `report` column as the flexible width.
- Each row renders `v.report_message` truncated to fit, or `—` when absent.
- There is no dedicated roster cost column; completed-task cost still appears in
  completion transcript/status events where relevant.

**Current behavior:** each roster row carries the latest one-line `report` teaser
in place of the former `$` figure.

## 3. Compact single-line builtin tool-call rendering

Tool calls in the verbose transcript render as compact one-line
`tool(arg1, arg2)` forms when the arguments fit and can be summarized.

- The `TranscriptItem::ToolCall` render arm first hides internal tools, then
  tries `render_file_edit_call`, then `compact_tool_call_line`, and only falls
  back to the monospace JSON block when compact rendering is unsuitable.
- `compact_tool_call_line` parses JSON args, summarizes common builtin tools, and
  refuses lines that exceed the current render width.
- Builtin-specific compaction covers shell, file write, search/glob, web fetch,
  git, clipboard, and fleet worktree helpers.
- Tests cover positional single-arg rendering, shell command quoting, cwd display,
  shell polling, file-write content summaries, content search, clipboard ranges,
  large-arg fallback, width fallback, and no blank spacer between compact calls.

**Current behavior:** common tool calls render on one line with compact positional
or named arguments; oversized cases fall back cleanly.

## Relationship

- As-built cockpit: [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md).
- Cluster hub: [`fleet-tui.md`](./fleet-tui.md).

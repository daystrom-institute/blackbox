---
title: "Ratatui snapshot preview for Fleet TUI work"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - bro-harness
  - workspace-tools
brief: "A deterministic preview surface for Ratatui style work: render fleet screens and components at fixed terminal sizes so agents can inspect layout/chrome changes instead of inferring visuals from source diffs."
---

# Ratatui snapshot preview for Fleet TUI work

Fleet TUI edits are visual work. Today an agent can run `cargo check`, but cannot
see whether a roster header, composer border, bucket row, footer, or
single-agent transcript actually lines up. The missing primitive is a
deterministic preview path that renders Ratatui UI into an inspectable artifact
without taking over the operator's terminal.

## Desired operator workflow

For fleet-specific work, an agent should be able to run:

```bash
bro fleet snapshot --screen roster --fixture busy --width 120 --height 36
bro fleet snapshot --screen single-agent --fixture queued-input --width 100 --height 30
```

The command prints a stable text/ANSI frame to stdout by default and can
optionally write artifacts:

```bash
bro fleet snapshot --screen roster --fixture busy --format text --output /tmp/roster.txt
bro fleet snapshot --screen roster --fixture busy --format ansi --output /tmp/roster.ansi
```

Agents then inspect the rendered frame with ordinary file tools. CI can snapshot
the same output for regression tests.

## Scope

Phase 1 is not a general "run any TUI" harness. It is a code-owned preview entry
point for Blackbox's own Ratatui surfaces, starting with `bro fleet`.

Phase 1 screens:

- `roster`: roster body, provider/model/effort selector states, composer, footer.
- `single-agent`: transcript, activity strip, composer, queued-input banner.
- `component:<name>` later, once repeated chrome pieces are factored enough to
  preview in isolation.

Phase 1 fixtures:

- `empty`: no agents, dispatch composer visible.
- `busy`: Active, Waiting, Idle, Interrupted, and Finished rows, stable
  timestamps, latest report teasers, long names/models for truncation.
- `queued-input`: a focused single-agent view with local queued stdin and a
  transcript echo boundary.
- `tool-rendering`: compact tool calls, `file_edit` diff block, `shell_run`
  envelope result, todo state, and window-0 rider.

## Implementation shape

Add a small reusable render helper, owned by the CLI crate:

```rust
pub fn render_ratatui_text(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut ratatui::Frame<'_>),
) -> anyhow::Result<String>
```

Internally it uses `ratatui::backend::TestBackend` and `Terminal::draw`, then
serializes the backing buffer row by row. A sibling `render_ratatui_ansi` can
preserve style spans once text layout is stable.

For `bro fleet`, add a non-interactive subcommand rather than a flag on the live
TUI:

```text
bro fleet snapshot --screen <screen> --fixture <fixture> --width <cols> --height <rows> [--format text|ansi] [--output <path>]
```

The snapshot path must not load persisted fleet sessions and must not dispatch
providers. It should construct synthetic fixtures that exercise the same drawing
functions the live TUI uses. If a draw function currently requires live
`AgentHandle`s, split it at the display-model boundary so preview fixtures can
feed `AgentView`/row/transcript data directly.

## Harness and MCP boundary

Do not make bro-harness call blackboxd for this. The preview path is a local
binary capability: harness agents can use `shell_run` to invoke `bro fleet
snapshot`, and daemon-backed agents can do the same through their shell/workspace
tools. A later `tui_snapshot_preview` tool can wrap local preview commands, but
it should be a thin convenience over code-owned snapshot entry points, not a
daemon runtime dependency.

## Acceptance

- Fixed width/height render output is deterministic across runs.
- Preview commands never mutate fleet state, spawn agents, or read historical
  sessions unless an explicit fixture says so.
- Fleet roster and single-agent style edits can be validated by comparing a
  before/after text or ANSI artifact.
- Tests cover at least one roster and one single-agent fixture at narrow and
  wide terminal sizes.
- The helper is generic enough for future council/dashboard TUI previews.

## Open questions

- Whether ANSI snapshots are worth shipping in phase 1 or should wait until text
  layout snapshots prove useful.
- Where accepted snapshots should live if they become CI fixtures:
  `tests/snapshots/` is simplest; colocated `src/*_snapshots/` may keep TUI
  ownership clearer.
- Whether the semantic UI component locator gap should feed this surface by
  returning both edit points and matching preview fixture names.

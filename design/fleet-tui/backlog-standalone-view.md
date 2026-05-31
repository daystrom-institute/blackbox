---
title: "Fleet TUI — standalone single-agent view (v2, backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "A standalone shell that launches the harness directly into the reusable single-agent view component (transcript + steering composer + header) with no roster and no fleet chrome. The only context where /clear (fresh session) and a dedicated-view /resume are meaningful. Reuses the §5.4 component with no new model; deferred to v2 because the v1 win is multi-agent management."
---

# Fleet TUI — standalone single-agent view (v2, backlog)

> **Provenance.** Extracted from [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md)
> §5.5.

## Status / gate

**Deferred to v2.** The v1 win is **multi-agent management** (the fleet). The
standalone single-agent shell is a secondary surface; it reuses the same view
component, so it adds no new model — just a different entry/launch shape.

## The shape

The single-agent view (cockpit §5.4) is a **reusable component** — transcript +
steering composer + header. Fleet embeds it behind the roster (`→` to focus). A
**standalone** shell launches the harness directly into that component, with no
roster and no fleet chrome. It is the only context where:

- **`/clear`** (start a fresh session) is meaningful — in fleet, "new session" =
  dispatch a new agent, and resetting one = `Ctrl+X`+`Ctrl+X` + redispatch, so
  `/clear` is redundant there.
- **`/resume`** carries the "open a session into a dedicated single view" sense.

## Acceptance

- The harness can launch directly into the single-agent view component with no
  roster/fleet chrome.
- `/clear` starts a fresh session in that shell; `/resume` opens an existing
  session into the dedicated view.
- No new transcript/steering model — the §5.4 component is reused as-is.

## Relationship

- As-built component (§5.4) and parent (§5.5): [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md).
- Cluster hub: [`fleet-tui.md`](./fleet-tui.md).

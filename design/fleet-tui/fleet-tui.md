---
title: "Fleet TUI"
kind: design-hub
corpus: blackbox-design
topic:
  - fleet-tui
brief: "Nav hub for the Fleet TUI design cluster: bro fleet, a human cockpit for dispatching and live-driving many concurrent top-level entrypoint agents across providers over one bidirectional stream-json control protocol. Top-level abstraction — links the orchestration core as a library and spawns agents in-process; blackboxd is not in the execution path. Sorts the cluster into the shipped cockpit and a proposed backlog."
---

# Fleet TUI

`bro fleet` is a human cockpit for dispatching and live-driving many concurrent
**top-level entrypoint agents** across providers — GLM, DeepSeek, Brodex (all via
bro-harness) and Claude as first-class peers — over **one** control protocol (the
Claude Agent SDK bidirectional stream-json scheme).

**Top-level abstraction, not an orchestration sub-topic.** The TUI links the
orchestration core as a **library** and spawns agents **in-process**; the only
hard line is daemon RPC — no HTTP to a running `blackboxd`. It is a client of the
[bro-harness](../bro-harness/bro-harness.md) substrate, not of the daemon.

This page is the **nav waypoint** — start here, then follow a link.

## Shipped (as-built records)

- [Multi-provider agent cockpit](fleet-tui-cockpit.md) — the v1 cockpit and its
  full harness-side substrate: the bidirectional control protocol (interrupt /
  steer / `/compact`), the `report` tool, bounded tool results, in-process spawn,
  the `FleetOrchestrator` façade, the roster, navigation model, and verbose
  single-agent transcript. §7 is the as-built ledger; residuals point to backlog.
- [Window-0 diagnostics surfacing — Phase 1](fleet-window0-diagnostics-surfacing.md)
  — distinct rider rendering in the single-agent transcript.

## In flight / partial

- [Standalone single-agent view](backlog-standalone-view.md) — `bro agent` now
  launches directly into the single-agent component with no roster/fleet chrome;
  remaining work is runtime UX soak before promoting to as-built.

## Backlog (proposed — pick this up)

- [Builtin-tool rendering & roster UX polish](backlog-ux-polish.md) — animated
  activity throbber (executor + hidden classifier companion), roster
  cost-column→`report`-teaser, compact single-line tool-call rendering
  (← thread-068f07b4).
- [v1 follow-ons](backlog-follow-ons.md) — `@project` cwd + MCP config,
  input-history disk persistence, allocator probe-core extraction for headroom v2
  + capability badges, Alerting bucket + `/resume` of a deleted session.
- [Window-0 roster badge + Alerting derivation](backlog-window0-roster-alerting.md)
  — Phases 2–3: per-agent outstanding-errors badge + the acting-vs-ignoring
  `FleetState::Alerting` signal.
- [Named agents and peer mailbox](backlog-named-agent-messaging.md) — assign
  every roster-spawned agent a memorable `#Name` from a large pool and route
  peer messages through a fleet-local mailbox/switchboard.
- [Ratatui snapshot preview](ratatui-snapshot-preview.md) — deterministic
  fixed-size text/ANSI previews for roster, single-agent, and future component
  style work.
- [Standalone single-agent view (v2)](backlog-standalone-view.md) — launch the
  harness directly into the single-agent component, no roster/fleet chrome.

## Related

- The harness substrate this drives: [bro-harness](../bro-harness/bro-harness.md).
- Window-0 diagnostics engine: [bro-harness diagnostics](../bro-harness/bro-harness-diagnostics.md).

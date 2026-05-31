---
title: "Fleet TUI — v1 follow-ons (backlog)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "The follow-on items from the shipped v1 cockpit (fleet-tui-cockpit §7 items 8/9/10/15/16): the @project cwd + MCP-config map, per-agent input-history disk persistence, the allocator probe-core extraction for provider-headroom v2 + capability badges, and the fleet-state residue (Alerting-bucket supervision reuse + /resume of a deleted session). None are correctness gaps — the v1 cockpit is runnable; these extend it."
---

# Fleet TUI — v1 follow-ons (backlog)

> **Provenance.** Extracted from [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md)
> §7 (the ◑ partial / ○ follow-on items). The §7 ledger in that doc remains the
> as-built record; this is the actionable residue. Independently pickup-able.

## §7.8 — `@project` cwd map + MCP config (○ not built)

A TUI-local JSON config: an `@project` map (`keyword → absolute dir`) with
typeahead, **and** MCP server defs injected via `--mcp-config`
(`exec_args.rs:243`). Daemon-free; not the bbox project registry; no MCP
management UI in v1. **Today:** v1 uses the launch cwd / `--cwd`.
**Acceptance:** `@<keyword> <prompt>` resolves the new agent's cwd from the local
map (resolved fresh per dispatch, no stickiness); MCP defs flow via
`--mcp-config`.

## §7.9 — Per-agent input-history disk persistence (◑ partial)

Per-agent history of the user's inputs, recallable in the single-agent view
(`↑/↓`). **Today:** in-memory recall is implemented (single-agent ↑/↓,
down-to-clear); on-disk persistence to the cockpit's `store_dir` is not.
**Acceptance:** input history survives a cockpit restart for each agent.

## §7.10 — Allocator probe-core extraction → provider-headroom v2 + badges (○/◑)

Extract the allocator probe core to a shared crate (`ProbeStore`/`ProbeRecord` +
`quota_capacity`, `allocator.rs:360,939,1286,1293`) so the cockpit links it and
writes its **own** probe store from its own dispatch rate-limit telemetry +
on-demand probe — daemon-free. Feeds the **provider-selector v2** (headroom-aware
routing) and **capability badges** on the roster. **Today:** the v1 selector
text-cycles the provider list with a flashing `next:` indicator; glyph / tag /
grouping / per-provider cost are done. **Acceptance:** the selector ranks
providers by live headroom; capability badges render per provider.

## §7.16 residue — Alerting bucket + `/resume` of a deleted session (◑ partial)

The fleet-state taxonomy is mostly built (Waiting/Idle/Active/Interrupted derived
from the stream; `Ctrl+X` stop→delete; on-roster Interrupted sessions resume on
steer). Residual:

- **Alerting-bucket supervision reuse** — wire the `Alerting` state to a real
  signal. (The window-0 acting-vs-ignoring derivation in
  [`backlog-window0-roster-alerting.md`](./backlog-window0-roster-alerting.md) is
  one such signal; supervision telemetry is another.)
- **`/resume` of a *deleted* session** — re-open a session that was stopped and
  removed from the roster, not just an Interrupted one still present.

**Acceptance:** an agent can enter `Alerting` from a real persisted-problem signal;
a deleted session can be re-opened via `/resume`.

## Relationship

- As-built ledger: [`fleet-tui-cockpit.md`](./fleet-tui-cockpit.md) §7.
- Cluster hub: [`fleet-tui.md`](./fleet-tui.md).

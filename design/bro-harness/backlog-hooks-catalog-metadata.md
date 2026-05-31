---
title: "bro-harness hooks — catalog-metadata nudge channel (v2)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "The v2 evolution of the Nudger: move nudge rules out of the harness's static table into catalog metadata. Atom/SM/tool descriptors carry optional nudge_triggers; the daemon compiles the active set into a blob injected at dispatch. The harness stays a generic engine that knows nothing about specific atoms. Deliberately gated on the §6 adoption loop proving the v1 engine earns its keep."
---

# bro-harness hooks — catalog-metadata nudge channel (v2)

> **Provenance.** Extracted from [`bro-harness-hooks.md`](./bro-harness-hooks.md)
> §4 ("Rule source — phased", item 2) and Build-order step 4. The v1 engine
> (HookEngine + NudgeLedger + Delivery + four harness-shipped rules) is built;
> this is the deferred v2 rule-source channel.

## Status / gate

**Do not build until the §6 adoption loop shows the engine earns its keep.** The
v1 harness-shipped static rule table exists precisely so the engine and the
adoption-measurement loop can be validated *before* investing in a metadata
channel. This item is the v2 that follows positive adoption signal.

## Problem

Hardcoding `regex → atom` rules in the harness rots the moment the atom catalog
changes, and violates the project's "system memories are signposts, not ledgers"
rule. The source of truth for what an atom is, and when to steer toward it,
should live with the atom — not duplicated in a harness static table.

## Approach

- Atom / system-memory / tool descriptors gain an optional `nudge_triggers`
  metadata field (behavioral or lexical trigger spec + the steer-toward target).
- The daemon compiles the active set into a blob injected at dispatch time, using
  the same channel discipline as `--mcp-config` (dispatch-time injection, not a
  runtime backchannel — preserves the no-daemon-runtime-dependency invariant).
- The harness stays a **generic** engine: it evaluates whatever rule blob it was
  handed and knows nothing about specific atoms. Adding an atom can ship its own
  nudge without touching the harness.
- Source of truth stays in `atom_search` / `atom_describe`.

## Acceptance

- A new atom ships a `nudge_triggers` entry and its nudge fires in a harness
  dispatch **with no change to the harness crate**.
- The harness engine contains no atom-specific rule constants after migration of
  the four v1 rules into catalog metadata.
- The compiled blob rides the dispatch injection path, not an MCP/RPC call from
  harness to daemon.

## Relationship

- Parent / v1 engine: [`bro-harness-hooks.md`](./bro-harness-hooks.md).
- Cluster map: [`bro-harness.md`](./bro-harness.md).

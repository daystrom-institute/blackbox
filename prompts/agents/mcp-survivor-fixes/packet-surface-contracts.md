---
title: "Packet and MCP policy discovery bounds and validation"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A03, A05, A06, A10, A13 within this assigned MCP caller surface."
---

# Packet and MCP policy discovery bounds and validation

Read [the shared execution contract](README.md) first. Audit findings: A03, A05, A06, A10, A13.

Owned implementation: src/tools/packets.rs; src/tools/mcp_surface.rs; crates/bbox-packets/src/events.rs and directly related packet response modules/tests. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

An invalid bbox_packet_events operation returned successful empty results. Surface replay repeats policy lists beside visible_tools; list/describe inventories are unpaged. Latest-N event retrieval lacks a clear older-page path. Exact packet body paging already reconstructed correctly in the audit.
- Reject invalid closed packet-event operations before lookup. Preserve genuine free-text filters as free text.
- Bound event history with an older-page path, explicit ordering, totals/continuation, and honest mutable-view semantics. Preserve the working exact packet body reader and its cursor guarantees.
- Make MCP surface list/describe/replay compact by default, avoiding duplicate policy inventories. Provide bounded exact policy/tool-list detail and reject action-irrelevant selectors or clearly express them in schema.
- Bound packet apply-all/audit nested findings. Reject oversized effectful batches before effects where applicable; preserve per-item outcomes and disclose observation-event writes.
- Improve accepted packet input/dataset shape or actionable validation without copying the whole AST manual into chooser prose. Correct the arbitrary-scale claim and distinguish dataset agreement from general correctness.
- Write isolated invalid-op, large event/inventory/findings, complete recovery, stale cursor, oversized-before-effects, and normal replay-deny tests. Do not redesign the packet engine or MCP catalog/filter plane.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

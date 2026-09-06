---
title: "Bounded graph discovery and exact schema detail"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A04, A06, A10, A13 within this assigned MCP caller surface."
---

# Bounded graph discovery and exact schema detail

Read [the shared execution contract](README.md) first. Audit findings: A04, A06, A10, A13.

Owned implementation: src/tools/graph.rs; src/project_graph_read.rs; directly related graph read DTOs/tests. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

Live bbox_project_graph_describe returned 26,237 result bytes. project_graph_describe_domain clones the complete schema (9,229 compact bytes) into the default description. Graph inventory and validation error lists can also grow.
- Default describe to a compact useful summary with graph identity, generation, authority, retrieval state, schema counts, and a discoverable exact schema/detail reader.
- Reuse existing response/body-page primitives for exact JSON recovery with selector/content-bound cursors. Preserve all schema bytes through continuation, Unicode included, and reject stale or cross-graph cursors.
- Bound graph list and validation findings with totals, continuation, and recoverable details. Keep both graph families and unavailable/stale/partial states distinguishable.
- Write synthetic large-schema and large-error fixtures, complete-page reconstruction and JSON parse checks, empty states, and stale cursor tests. Assert serialized response budgets, not only item counts.
- Update only affected graph parameter/chooser stanzas and keep hot descriptions short. Do not alter graph publication, source generations, index storage, query ranking, or graph construction.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

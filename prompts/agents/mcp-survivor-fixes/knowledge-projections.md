---
title: "Useful knowledge summaries and exact diagnostic recovery"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A04, A05, A06, A08, A13 within this assigned MCP caller surface."
---

# Useful knowledge summaries and exact diagnostic recovery

Read [the shared execution contract](README.md) first. Audit findings: A04, A05, A06, A08, A13.

Owned implementation: src/tools/knowledge.rs; crates/bbox-knowledge/src/knowledge.rs; directly related read projection tests. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

A no-match global knowledge query with limit=1 returned 6,789 result bytes, mostly visibility diagnostics repeated as text and structured content. Exact system-memory/knowledge bodies and review queue records can also be unbounded.
- Make the default reply prioritize matches plus compact scoped warning/count summaries. Preserve unavailable, stale, queued, and partial state. Avoid repeating whole diagnostics in both representations.
- Add bounded opt-in exact diagnostic recovery that preserves the original query scope. Narrowing the project is not a substitute for recovering omitted global diagnostics.
- Bound oversized exact knowledge/system-memory reads and review queue records with discoverable continuation, provenance and stable/content-bound cursors. Preserve progressive ranking and bounded packet/system-memory sidecars.
- Write tests for no results with many diagnostics, oversized one-record bodies, Unicode reconstruction, review pagination, stale cursors, and warning preservation.
- Trace bbox_absorb and bbox_bootstrap consumers. Correct misleading chooser/outcome claims for inert compatibility actions; do not remove callable names or stored data solely on this audit recommendation. Report a concrete consumer-backed retirement disposition separately.
- Leave queued owner delivery, merge/CAS, source publication, and knowledge ranking unchanged. Update only this family's public schema/chooser stanzas.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

---
title: "Maintenance selector parity and bounded diagnostic output"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A03, A06, A09, A10, A13, A14 within this assigned MCP caller surface."
---

# Maintenance selector parity and bounded diagnostic output

Read [the shared execution contract](README.md) first. Audit findings: A03, A06, A09, A10, A13, A14.

Owned implementation: src/tools/storage_migration.rs; src/tools/storage_health.rs; src/tools/doctor.rs; directly related maintenance projection modules/tests. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

Storage migration dry-run compares raw selectors only to registered project IDs while apply uses project resolution, despite advertised path selectors. Maintenance inventories and full doctor detail can be unbounded; doctor builds a full report before filtering sections.
- Resolve dry-run/apply selectors through the same existing authoritative contract. Exercise IDs, aliases, managed/historical paths where supported, missing selectors, and unknown projects. Refusal must be consistent when authority is unavailable.
- Validate maintenance action/section vocabulary before scans. Include supported scrub in partition schema prose, if still served, and document real local/offline prerequisites.
- Bound migration plans, partition inventories and doctor detail with totals/continuation or content-bound exact pages. Preserve repair hints and partial/unavailable states.
- Where a requested doctor section has an existing independent producer, avoid constructing unrelated expensive sections. Do not invent new scan caches or background jobs.
- Write isolated selector parity, invalid section/action, large single/nested record, recovery and focused producer-invocation tests. Do not execute migration, scrub, repair, or doctor against production.
- Leave storage engines, compaction algorithms, GC authority, source transport and publication unchanged. Do not implement new remote maintenance transport.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

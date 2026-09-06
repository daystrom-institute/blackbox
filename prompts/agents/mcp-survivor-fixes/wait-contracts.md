---
title: "Aggregate task selection and outcome truth"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A01, A06, A12 within this assigned MCP caller surface."
---

# Aggregate task selection and outcome truth

Read [the shared execution contract](README.md) first. Audit findings: A01, A06, A12.

Owned implementation: src/tools/dispatch.rs; aggregate-only helpers in src/orchestration/mod.rs. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

The live audit called bro_when_all with one unknown ID and received all_completed=true with no results. bro_when_any silently dropped the same ID, while bro_wait correctly rejected it. resolve_when_targets accepts IDs without existence validation; filter_map erases missing records.
- Validate the entire selection before waiting. Reject ambiguous team plus task_ids, empty selections, unknown IDs, and unsupported selectors with actionable errors. Deliberately define duplicate IDs and pruned team-history IDs.
- Preserve the distinction between every task being terminal and every task succeeding. Mixed success, failure, cancellation, running, and timeout must retain each selected task's outcome.
- Bound input fanout before observing/waiting and bound aggregate output including per-task bodies. Reuse bro_status exact-result retrieval; never silently omit requested tasks or change a timeout into success.
- Write isolated tests for all-missing, mixed known/missing, duplicate, competing selectors, stale team history, empty team, mixed terminal outcomes, and oversized fanout. Review adjacent broadcast aggregation and fix the same omission/budget defect if present without changing dispatch mechanics.
- Update only the affected wait/broadcast schema and chooser stanzas. Do not rewrite the task store, task admission, or replay delivery.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

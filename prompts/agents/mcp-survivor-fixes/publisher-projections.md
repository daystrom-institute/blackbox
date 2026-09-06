---
title: "Compact publisher status with retained authority evidence"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A05, A06, A09, A10, A13 within this assigned MCP caller surface."
---

# Compact publisher status with retained authority evidence

Read [the shared execution contract](README.md) first. Audit findings: A05, A06, A09, A10, A13.

Owned implementation: src/tools/project_catalog.rs (publisher status and its read-only DTO/helpers/tests only). Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

Publisher status returned 4,135 bytes repeating accepted scope/ref/commit/generation/binding below health. Larger connector observations and health inventories can grow. Its generation_id and pointer_sha256 CAS tokens are useful and must survive projection.
- Provide a compact default publisher status retaining canonical identity, current generation/pointer tokens, source binding, and actionable stale/unavailable/queued/partial health.
- Deduplicate repeated identity/acceptance evidence. Add bounded opt-in exact diagnostic detail with selector/content-bound continuation.
- Bound connector/health nested inventories and preserve total/omission counts. Do not present recorded observations as current filesystem authority.
- Shorten chooser prose to purpose, selectors, prerequisites and detail recovery; move necessary deep explanation into an existing scoped document if needed.
- Write synthetic large health/connector fixtures, missing publisher, stale/partial status, CAS token preservation, exact reconstruction and changed-body cursor rejection tests.
- Own only publisher read projection. Do not alter catalog list/get already-paged contracts, attachment administration, owner source transport, accepted publication, queued checkout delivery, or catalog CAS mechanics.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

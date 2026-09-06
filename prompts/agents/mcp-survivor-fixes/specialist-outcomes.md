---
title: "Allocator persistence truth and safe specialist detail"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A04, A06, A10, A11, A12 within this assigned MCP caller surface."
---

# Allocator persistence truth and safe specialist detail

Read [the shared execution contract](README.md) first. Audit findings: A04, A06, A10, A11, A12.

Owned implementation: src/tools/agents.rs; src/orchestration/allocator.rs; agent/allocator read projection tests and directly related DTOs. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

These eight specialist tools were absent from this client's callable catalog, so findings are source-only. probe_store_save returns () and probe update/clear reports success without a save result. Agent get/describe and allocator inventories/traces expose potentially large raw manifests/resolved configuration.
- Reproduce failed probe persistence with an isolated fixture, then propagate the actual write failure to the caller. Successful admission must not claim durable persistence that failed. Preserve legitimate successful update/clear behavior.
- Bound specialist inventories and individual manifest/trace/probe bodies. Expose compact useful identity/status plus bounded exact detail with content/selector-bound cursors.
- Use synthetic secret sentinels in accepted credential/opaque-config fields and nested/debug/error views. Fix proven disclosure without treating every free-text field as a credential.
- Make describe distinguish stored manifest, resolved brofile/overlay, and runtime filter planes it has not computed. Keep readiness/missing-capability errors actionable.
- Write failed-save, successful-save, oversized manifest/trace, redacted reconstruction, stale cursor and synthetic secrecy tests. Report whether evidence is unit/source/live; do not claim a live probe of an unavailable tool.
- Keep allocator selection algorithms, provider routing, registration inventories and fleet settings unchanged. No new database or generic persistence subsystem; this is error propagation and response projection.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

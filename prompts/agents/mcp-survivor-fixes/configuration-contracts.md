---
title: "Honest brofile and MCP configuration actions"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A03, A04, A06, A08, A09, A10, A11 within this assigned MCP caller surface."
---

# Honest brofile and MCP configuration actions

Read [the shared execution contract](README.md) first. Audit findings: A03, A04, A06, A08, A09, A10, A11.

Owned implementation: src/tools/roster.rs (brofile/account branches only); src/tools/config.rs; src/orchestration/brofile.rs; src/orchestration/mcp.rs. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

Invalid scope in bro_brofile list returns normal results; get ignores scope. bro_mcp list ignores its scope, sync has FANOUT_PROVIDERS=[], and stdio add advertises a path that then points at retired provider CLIs.
- Validate closed scope/action vocabularies and action-specific required fields. Make requested store selection effective for list/get or explicitly reject unsupported combinations. Do not widen typos to effective/global lookup.
- State actual config owner/prerequisites and supported transport lanes. Remove obsolete provider/Gemini/CLI guidance. A destination-free sync must return an honest unsupported/retired outcome without resolving secrets or pretending to synchronize.
- Bound account inventories and large brofile/config detail. Supply exact recovery after redaction, with selector/content-bound cursors and useful presence/identity summaries.
- Add synthetic sentinel tests for env, headers, endpoint credentials, nested config, errors, and text/structured/debug views. Preserve safe identity and presence fields; do not blindly redact ordinary prose.
- Test scope parity, unknown scope, ignored fields, no-op sync, large inventories, full redacted reconstruction, and existing valid config operations.
- Do not change team lifecycle branches, dispatch provider routing, fleet configuration, real accounts, or running services. Callable-name removal is a separate consumer disposition, not part of this assignment.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

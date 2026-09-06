---
title: "Pin and note identity plus bounded thread recovery"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A02, A03, A04, A06, A10, A13 within this assigned MCP caller surface."
---

# Pin and note identity plus bounded thread recovery

Read [the shared execution contract](README.md) first. Audit findings: A02, A03, A04, A06, A10, A13.

Owned implementation: src/tools/attention.rs; src/tools/notes.rs; src/tools/threads.rs; crates/bbox-stores/src/pins.rs; crates/bbox-threads/src/{threads,notes}.rs. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

The live pin list for a registered project fails attachment_inactive because attention.rs resolves checkout write authority before branching. Thread get returned 23,108 result bytes with no continuation. Pin invalid scope yields a successful empty result.
- Separate host-owned pin/note project association from checkout-owned writes. Use existing catalog/filter resolution for read identity and explicitly define unknown selector behavior. Do not add checkout transport or duplicate resolver logic.
- Validate actions, scopes, and required fields before locality work. Exercise registered remote project, logical project without attachment, alias, historical path, and unknown selector.
- Make thread get useful by default with current handoff/checkpoint and counts; provide bounded exact history/detail recovery. Add bounded pin/note list bodies and exact reads so a single large body remains recoverable.
- Reuse existing content-bound body cursors, reject stale or cross-selector cursors, and label live offset pagination honestly. Preserve stored content and existing mutation semantics.
- Write isolated tests for the identity cases, invalid enums, empty lists, Unicode large bodies, full reconstruction, and stale continuation. Update corresponding public params and chooser stanzas, including thread get.
- Leave ambient pin storage ownership and checkout knowledge delivery machinery intact. Coordinate any necessary resolver API change by reporting it; consume established helpers first.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

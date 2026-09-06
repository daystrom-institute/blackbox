---
title: "Entity-aware search follow-up calls"
kind: agent-lens
corpus: blackbox-prompts
audience: dispatched-bro
topic: [prompts, prompts-agents, mcp]
brief: "Implement A07 within this assigned MCP caller surface."
---

# Entity-aware search follow-up calls

Read [the shared execution contract](README.md) first. Audit findings: A07.

Owned implementation: crates/bbox-corpus-index/src/index/search.rs and its directly related search result tests/types. Matching parameter definitions, isolated tests, and named-tool documentation stanzas are included; unrelated branches of shared files are not.

## Required change and acceptance

Searching for the audit phrase returned a thread, then suggested bbox_context using its thread-store locator and bbox_messages with an empty session_id. Both follow-ups failed. search_with_project_filter remembers transcript-like coordinates and only distinguishes Slack from other hits.
- Carry the actual entity kind and canonical reference into recovery hint selection. Thread hits should direct callers to bbox_thread get or the supported entity reader; other non-transcript hits need their matching supported reader.
- Emit transcript context/session hints only when the hit is a transcript with valid indexed coordinates. Keep source freshness and indexed_projection_only semantics intact.
- Inspect each existing hit family (native transcript, thread, knowledge, code, Slack and generic entity) and choose supported recovery or honestly state unavailable recovery. Never expose a daemon-local locator as a caller-local file.
- Write synthetic regression tests that assert generated tool names and required selectors, including blank session, missing coordinates, and a working transcript positive control.
- Keep ranking, recall, indexing, and reader validation unchanged. Do not rebuild the search index or weaken fail-closed readers. Update only search chooser documentation if needed.

## Deliverable

Implement the acceptance cases, write tests without compiling locally, use the pinned formatter, commit and push the assigned branch. Report the SHA, tests written versus checks run, compatibility implications, and unresolved cross-owner dependencies. Do not close the audit gap.

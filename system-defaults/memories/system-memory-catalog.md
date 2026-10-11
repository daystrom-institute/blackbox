---
title: System Memory Catalog
brief: Plain Markdown map for navigating system memory runbooks from Obsidian.
---
# System Memory Catalog

Plain Markdown map for navigating the system memory runbooks from Obsidian.
This note is not loaded as a runtime `sm-*` system memory.

Files in `system-defaults/memories/` use bare slugs such as
`gap-notes.md`; runtime IDs use the `sm-` prefix, such as
`sm-gap-notes`.

## Retrieval And Corpus Navigation

- [sm-agentic-opening-sequence](agentic-opening-sequence.md) - first-loop
  grounding across knowledge recall, search, and entity inspection.
- [sm-transcript-retrieval](transcript-retrieval.md) - transcript search,
  citation, context, session, and message retrieval ladders.

## Knowledge, Persistence, And Render Hygiene

- [sm-persistence-taxonomy](persistence-taxonomy.md) - when to learn
  (rendered or recall-only) or keep context in a thread.
- [sm-render-lifecycle](render-lifecycle.md) - learn and render lifecycle.
- [sm-create-etiquette](create-etiquette.md) - list-before-create dedupe
  discipline.
- [sm-gap-notes](gap-notes.md) - reporting missing substrate capabilities.

## Bro execution and historical records

- [sm-bro-dispatch-patterns](bro-dispatch-patterns.md) - exec, resume, wait,
  race, and deliberation patterns for bro dispatch.
- [sm-brofile-context](brofile-context.md) - brofile context policy,
  provider-default suppression, and minimal probe validation.
- [sm-workflow-orchestration](workflow-orchestration.md) - caller composition and historical workflow records.
- [sm-atoms](atoms.md) - retired atom execution and replacements.

## How To Use

Open this file in Obsidian and follow regular Markdown links to the target
runbooks. Keep detailed procedures in the target runbook; update this file only
when adding, removing, or renaming a system memory.

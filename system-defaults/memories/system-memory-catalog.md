---
title: System Memory Catalog
brief: Plain Markdown map for navigating system memory runbooks from Obsidian.
---
# System Memory Catalog

Plain Markdown map for navigating the system memory runbooks from Obsidian.
This note is not loaded as a runtime `sm-*` system memory.

Files in `system-defaults/memories/` use bare slugs such as
`rule-packets.md`; runtime IDs use the `sm-` prefix, such as
`sm-rule-packets`.

## Retrieval And Corpus Navigation

- [sm-agentic-opening-sequence](agentic-opening-sequence.md) - first-loop
  grounding across schema, search, entity inspection, path finding, and
  evidence bundles.
- [sm-transcript-retrieval](transcript-retrieval.md) - transcript search,
  citation, context, session, and message retrieval ladders.

## Knowledge, Persistence, And Render Hygiene

- [sm-persistence-taxonomy](persistence-taxonomy.md) - when to learn,
  remember, decide, note, or pin.
- [sm-render-lifecycle](render-lifecycle.md) - render, absorb, review, and
  lint lifecycle.
- [sm-scoped-pins](scoped-pins.md) - hot context for one active execution lane.
- [sm-side-channel-notes](side-channel-notes.md) - executor and orchestrator
  note emission.
- [sm-create-etiquette](create-etiquette.md) - list-before-create dedupe
  discipline.
- [sm-gap-notes](gap-notes.md) - reporting missing substrate capabilities.

## Packets And Deterministic Judges

- [sm-rule-packets](rule-packets.md) - compile reusable mechanisms from
  examples or rules.
- [sm-review-packets](review-packets.md) - code review and PR triage packet
  patterns.
- [sm-auth-packets](auth-packets.md) - authorization and access-table packet
  patterns.
- [sm-design-packets](design-packets.md) - design proposal ranking and
  iteration packets.

## Agents, Atoms, Workflows, And Whiteboards

- [sm-bro-dispatch-patterns](bro-dispatch-patterns.md) - exec, resume, wait,
  race, and deliberation patterns for bro dispatch.
- [sm-workflow-orchestration](workflow-orchestration.md) - daemon-owned
  multi-phase workflow arcs.
- [sm-atoms](atoms.md) - public reusable capability contracts and invocation
  semantics.
- [sm-whiteboards](whiteboards.md) - multi-agent deliberation boards.

## Refactor Mechanization

- [sm-refactor](refactor.md) - language routing, shared refactor surfaces, and
  matrix.
- [sm-refactor-rust](refactor-rust.md) - Rust tree-sitter inventory, writable
  extraction, and rust-analyzer-backed plan rules.
- [sm-refactor-java](refactor-java.md) - Java tree-sitter inventory, JDT
  validation, and Java plan routing.
- [sm-refactor-java-extract-class](refactor-java-extract-class.md) - composite
  Java class extraction, capture analysis, and generated FIXME catalog.
- [sm-refactor-java-lombokify](refactor-java-lombokify.md) - Java POJO
  boilerplate to Lombok annotations.
- [sm-refactor-typescript](refactor-typescript.md) - TypeScript and JavaScript
  inventory and validation workflow.
- [sm-refactor-python](refactor-python.md) - Python inventory and Pyright/Rope
  validation workflow.
- [sm-refactor-csharp](refactor-csharp.md) - C# inventory and Roslyn
  validation workflow.
- [sm-refactor-go](refactor-go.md) - Go inventory and gopls validation
  workflow.
- [sm-refactor-c-cpp](refactor-c-cpp.md) - C/C++ inventory and clang
  validation workflow.

## How To Use

Open this file in Obsidian and follow regular Markdown links to the target
runbooks. Keep detailed procedures in the target runbook; update this file only
when adding, removing, or renaming a system memory.

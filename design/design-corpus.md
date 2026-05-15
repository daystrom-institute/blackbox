---
title: "Design Corpus"
kind: design-hub
corpus: blackbox-design
topic:
  - design-corpus
brief: "Top-level map for the Blackbox design corpus, with topic hubs and lifecycle guidance."
---

# Design Corpus

This directory holds design records for Blackbox. Treat it as a work-tracking
corpus, not as the authority for current runtime behavior. When a design
describes behavior that matters for implementation, verify it against the code,
`PROJECT.md`, and current tests before relying on it.

## Topic Hubs

- [Orchestration](orchestration/orchestration.md) - atoms, agents, workflows,
  supervision, phase decomposition, and runtime handoff.
- [Refactor Tools](refactor-tools/refactor-tools.md) - structural refactor
  machinery, refactor atoms, Rust expansion, and Java gap closure.

## Lifecycle

Lifecycle is now metadata, not the primary filing system:

- `lifecycle: proposed` - candidate designs and not-yet-accepted directions.
- `lifecycle: partial` - in-flight designs or implementation plans where some
  work has landed and some remains.
- `lifecycle: archived` - shipped, closed, superseded, or historical designs.

The old `proposed/`, `partial/`, and `archive/` directories remain for documents
not yet migrated into topic homes.

## Maintenance Notes

- Prefer updating the source design doc over summarizing details here.
- Put new design docs in the topic hierarchy when the topic is obvious.
- Use a descriptive hub-note filename for each hierarchy unit. Avoid generic
  `INDEX.md` files; they create low-signal graph nodes in Obsidian.

---
title: "Atom Capability Runtime"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
  - atoms
brief: "Crosscut hub for atoms as public, contracted capabilities over brofiles, workflows, deterministic runners, and adapters."
---

# Atom Capability Runtime

Atoms are the public capability boundary. Bros remain runtime workers; agents
are the predecessor discovery wrapper; workflows can expose reusable capability
boundaries through atom bindings.

## Core Docs

- [Atom System](atom-system.md)
- [Atom System - Implementation Plan](atom-system-impl.md)
- [Agent System](../agents/agent-system.md)
- [Agent System - Implementation Skeleton](../agents/agent-system-impl.md)

## Crosscuts

- [Workflow Orchestration](../workflows/workflow-orchestration.md)
- [Supervised Execution](../supervision/supervised-execution.md)
- [Large Work Decomposition](../phase-decomposer/large-work-decomposition.md)
- [Refactor Agents](../../refactor-tools/refactor-agents.md)
- [Rust Refactor Atoms - Batch 2](../../refactor-tools/rust/rust-refactor-atoms-batch2.md)

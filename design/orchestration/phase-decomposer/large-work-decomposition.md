---
title: "Large Work Decomposition"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
  - phase-decomposer
brief: "Hub for context-budget-aware decomposition of large phase docs into scouted, sized, parallelizable work."
---

# Large Work Decomposition

The phase decomposer fits large work to provider context budgets by scouting the
actual evidence load, measuring it, then choosing direct implementation or
decomposed parallel execution with recomposition.

## Docs

- [Phase Decomposer](phase-decomposer.md)
- [Phase Decomposer - Implementation Plan](phase-decomposer-impl.md)
- [Archived Phase Decomposer v0](phase-decomposer-archived.md)

## Crosscuts

- [Supervised Execution](../supervision/supervised-execution.md)
- [Workflow Orchestration](../workflows/workflow-orchestration.md)
- [Atom Capability Runtime](../atoms/atom-capability-runtime.md)

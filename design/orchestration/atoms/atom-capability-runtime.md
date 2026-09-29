---
title: "Atom Capability Runtime"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
  - atoms
tags:
  - atoms
  - refactor-tools
brief: "Crosscut hub for the atom and agent capability designs over brofiles, workflows, deterministic runners, and adapters."
---

# Atom Capability Runtime

This cluster records the atom and agent capability designs. Blackbox dispatches
ordinary bros configured by brofiles and teams; callers own higher-order
composition, as defined by the
[bro execution boundary](../bro-execution-boundary-and-retirement.md).

## Core Docs

- [Atom System](atom-system.md)
- [Atom System - Implementation Plan](atom-system-impl.md)
- [Agent System](../agents/agent-system.md)
- [Agent System - Implementation Skeleton](../agents/agent-system-impl.md)

## Crosscuts

- [Workflow Orchestration](../workflows/workflow-orchestration.md)
- [Supervised Execution](../supervision/supervised-execution.md)
- [Refactor Agents](../../refactor-tools/refactor-agents.md)
- [Rust Refactor Atoms - Batch 2](../../refactor-tools/rust/rust-refactor-atoms-batch2.md)

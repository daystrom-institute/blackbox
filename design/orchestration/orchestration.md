---
title: "Orchestration"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
brief: "Hub for Blackbox orchestration designs: the bro execution boundary, supervision, phase decomposition, live handoff, and historical atom, agent and workflow records."
---

# Orchestration

This hub groups designs for starting, supervising, composing, and recovering
agentic work.

The [bro execution boundary](bro-execution-boundary-and-retirement.md) defines
the split: Blackbox provides reliable bro execution and corpus capabilities, and
callers own higher-order orchestration. The atom, agent, workflow and supervision
designs below are historical records; the boundary doc records dependencies,
preserved capabilities, migration contracts and verification gates.

## Capability Runtime

- [Atom System](atoms/atom-system.md)
- [Atom System - Implementation Plan](atoms/atom-system-impl.md)
- [Agent System](agents/agent-system.md)
- [Agent System - Implementation Skeleton](agents/agent-system-impl.md)
- [Consultant Runtime - Badgey dissolution](agents/consultant-runtime.md)
- [Atom Capability Runtime](atoms/atom-capability-runtime.md)

## Provider execution and control

- [Bro-Harness](../bro-harness/bro-harness.md): the standalone coding-agent runtime.
- [Task control plane](../daemon-runtime/control-plane.md): dispatch, task
  observation and managed-worktree closeout.

## Workflow Execution

- [Workflow Orchestration](workflows/workflow-orchestration.md)
- [Turing Completeness](workflows/turing-completeness.md)

## Supervised Execution

- [Supervised Execution](supervision/supervised-execution.md)
- [Supervision](supervision/supervision.md)
- [Supervision Implementation Notes](supervision/supervision-impl.md)
- [Mechanical Supervision Telemetry](supervision/supervision-mechanical.md)
- [Classifier Co-session](supervision/supervision-classifier-cosession.md)
- [Turn-end Advisor](supervision/supervision-turn-end-advisor.md)
- [Supervision Phased Implementation](supervision/supervision-phased-implementation.md)
- [Runtime Allocation Tier Mapping](supervision/runtime-allocation-tier-mapping.md)

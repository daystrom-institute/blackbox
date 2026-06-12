---
title: "Orchestration"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
brief: "Hub for Blackbox orchestration designs: atoms, agents, workflows, supervision, phase decomposition, and live handoff."
---

# Orchestration

This hub groups designs for starting, supervising, composing, and recovering
agentic work.

## Capability Runtime

- [Atom System](atoms/atom-system.md)
- [Atom System - Implementation Plan](atoms/atom-system-impl.md)
- [Agent System](agents/agent-system.md)
- [Agent System - Implementation Skeleton](agents/agent-system-impl.md)
- [Consultant Runtime - Badgey dissolution](agents/consultant-runtime.md)
- [Atom Capability Runtime](atoms/atom-capability-runtime.md)

## Provider Transport & Cockpit (moved out)

The custom provider harness and the fleet cockpit are **top-level** design
clusters now — daemon-independent abstractions, not orchestration sub-topics. See
the [Design Corpus](../design-corpus.md) Topic Hubs:

- [Bro-Harness](../bro-harness/bro-harness.md) — the custom headless agent
  (transports, tool surface, clipboard, chaining, hooks, diagnostics, neuralyze).
- [Fleet TUI](../fleet-tui/fleet-tui.md) — `bro fleet`, the multi-provider
  live-driving cockpit.

## Workflow Execution

- [Workflow Orchestration](workflows/workflow-orchestration.md)
- [Brofile Context Templates](workflows/context-assembly-system.md)
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

## Large Work Decomposition

- [Large Work Decomposition](phase-decomposer/large-work-decomposition.md)
- [Phase Decomposer](phase-decomposer/phase-decomposer.md)
- [Phase Decomposer - Implementation Plan](phase-decomposer/phase-decomposer-impl.md)
- [Archived Phase Decomposer v0](phase-decomposer/phase-decomposer-archived.md)

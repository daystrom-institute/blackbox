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
- [Atom Capability Runtime](atoms/atom-capability-runtime.md)

## Provider Transport

- [Anthropic-shaped Custom Harness (bro-harness)](anthropic-harness.md)

## Harness Tool Surface

- [**bro-harness Design Map** (start here — dependency graph + build order)](bro-harness.md)
- [bro-harness Tool Surface — Ideal Built-in Subset](bro-harness-tool-surface.md)
- [bro-harness Clipboard (clip_* registers)](bro-harness-clipboard.md)
- [bro-harness Tool Chaining (the ref ABI)](bro-harness-tool-chaining.md)
- [bro-harness Hooks & Nudges (ambient-meta seam)](bro-harness-hooks.md)
- [bro-harness Neuralyze (rewind + carry a message)](bro-harness-neuralyze.md)

## Workflow Execution

- [Workflow Orchestration](workflows/workflow-orchestration.md)
- [Brofile Context Templates](workflows/context-assembly-system.md)
- [Turing Completeness](workflows/turing-completeness.md)
- [Tmux Portal Workflows](workflows/tmux-portal-workflows.md)
- [Tmux Portal Workflows - Implementation Plan](workflows/tmux-portal-workflows-impl.md)

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

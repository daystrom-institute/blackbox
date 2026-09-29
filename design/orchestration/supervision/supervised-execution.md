---
title: "Supervised Execution"
kind: design-hub
corpus: blackbox-design
topic:
  - orchestration
  - supervision
brief: "Hub for atom-era supervision: mechanical telemetry, classifier observation, advisor judgment, and runtime recovery."
---

# Supervised Execution

This cluster records the supervision designs for atom and workflow execution:
mechanical telemetry, semantic classification and advisor recovery policy.
Retained execution measurements belong to bro execution, as defined by the
[bro execution boundary](../bro-execution-boundary-and-retirement.md).

## Docs

- [Supervision](supervision.md)
- [Supervision Implementation Notes](supervision-impl.md)
- [Mechanical Supervision Telemetry](supervision-mechanical.md)
- [Classifier Co-session](supervision-classifier-cosession.md)
- [Turn-end Advisor](supervision-turn-end-advisor.md)
- [Supervision Phased Implementation](supervision-phased-implementation.md)
- [Supervision Test Plan](supervision-test-plan.md)
- [Runtime Allocation Tier Mapping](runtime-allocation-tier-mapping.md)
- [acquire_drone](acquire-drone.md)

## Crosscuts

- [Atom Capability Runtime](../atoms/atom-capability-runtime.md)
- [Workflow Orchestration](../workflows/workflow-orchestration.md)
- [Large Work Decomposition](../phase-decomposer/large-work-decomposition.md)

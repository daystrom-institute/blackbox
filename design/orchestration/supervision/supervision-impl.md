---
title: "Supervision implementation notes"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - supervision
date: 2026-05-14
status: "compact implementation index"
brief: "Compact index for the atom-era supervision build sequence and its classifier/advisor implementation docs."
---

# Supervision implementation notes

The pre-atom implementation plan was retired because it treated classifier
co-session as daemon-side machinery and under-described the implemented
mechanical telemetry. The current design is split across:

- `design/orchestration/supervision/supervision-mechanical.md`
- `design/orchestration/supervision/supervision.md`
- `design/orchestration/supervision/supervision-classifier-cosession.md`
- `design/orchestration/supervision/supervision-turn-end-advisor.md`
- `design/orchestration/supervision/supervision-test-plan.md`
- `design/orchestration/supervision/supervision-phased-implementation.md` - active build
  sequence.

The phased implementation plan is the authoritative build order. The S-ids
below are stable labels for cross-document references:

1. **S1-normalize-plan.** Normalize atom manifest `supervision` plus workflow
   binding `supervision_override` into an internal `SupervisionPlan`. Promote or
   validate `supervision_override` as a typed shape instead of arbitrary JSON.
2. **S2-attachment-model.** Define observation attachment: the wrapper registers
   the primary invocation/task and grants classifier/advisor children scoped
   read-only observation or judge/action-proposal rights.
3. **S3-polling-primitive.** Add read-only polling primitives for attached
   atom/task status with bounded tails and full mechanical supervision snapshot
   access.
4. **S4-sleep-primitive.** Add a workflow sleep/timer primitive so polling loops
   do not hot-spin.
5. **S5-structured-exit.** Add structured workflow-backed atom exit output for
   classifier/advisor results.
6. **S6-classifier-atom.** Implement the classifier workflow-backed atom
   pattern.
7. **S7-advisor-atom.** Implement the advisor workflow-backed atom pattern.
8. **S8-action-executor.** Add a typed advisor action executor.
9. **S9-tier-recovery.** Integrate recovery lane selection with runtime
   allocation tier ladders.
10. **S10-tests.** Add the tests from
    `design/orchestration/supervision/supervision-test-plan.md`.

Do not reintroduce a daemon-special LLM oracle sidecar unless the reusable
workflow-backed atom model proves insufficient.

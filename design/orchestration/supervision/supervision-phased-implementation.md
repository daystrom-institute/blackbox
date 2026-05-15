---
title: "Supervision phased implementation plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - supervision
date: 2026-05-14
status: "partial implementation plan"
brief: "Active build sequence for reusable workflow-backed classifier and advisor atoms around supervised execution."
---

# Supervision phased implementation plan

Companion docs:

- `supervision.md`
- `supervision-mechanical.md`
- `supervision-classifier-cosession.md`
- `supervision-turn-end-advisor.md`
- `supervision-test-plan.md`
- `runtime-allocation-tier-mapping.md`

This plan implements supervised atom orchestration by adding reusable workflow
and atom-runtime primitives. It does not add a daemon-special LLM sidecar.

This is the active implementation sequence. `design/orchestration/supervision/supervision-impl.md`
is a compact S-id index for stable cross-document references; the table below
maps those S-ids to the phase plan. Test coverage is enumerated per phase here;
`design/orchestration/supervision/supervision-test-plan.md` remains the broader coverage
skeleton.

| Phase | Covers |
|---|---|
| P0 | new baseline verification work |
| P1 | S1-normalize-plan |
| P2 | S2-attachment-model, S3-polling-primitive |
| P3a | S5-structured-exit |
| P3b | S4-sleep-primitive |
| P4 | S6-classifier-atom |
| P5 | S7-advisor-atom |
| P6 | S8-action-executor |
| P7 | S9-tier-recovery |
| P8 | phase-decomposer integration |

## 1. Dependency graph

```text
P0 mechanical baseline verification
  |
P1 typed supervision plan
  |
P2 attachment and polling primitives
  |
P3a structured workflow exit
  |\
  | P3b workflow sleep
  |   |
  | P4 classifier workflow-backed atom
  |
P5 advisor workflow-backed atom
  |
P6 action executor + recovery attempts
  |
P7 runtime allocation integration
  |
P8 phase-decomposer integration
```

P0 is a recommended precondition for P1 because it locks down the telemetry
contract consumed by later polling. P1-P3a/P3b are runtime substrate. P4-P5
are reusable supervision artifacts. P6-P7 make the loop mutating. P8 composes
the loop into larger workflows.

## 2. P0 - Mechanical baseline verification

Goal: lock down the implemented telemetry before building policy on top of it.

Current code:

- `src/orchestration/supervision.rs`
- `src/orchestration/mod.rs`

Work:

- Keep `SupervisionState` as telemetry only.
- Add or verify tests that task status exposes green snapshots and full alert
  snapshots.
- Verify streaming providers call `observe_event`.
- Verify bulk providers call `observe_bulk_sink`.
- Document token-burn baseline seeding as optional until a real baseline source
  exists.

Deliverable: confidence that polling primitives can consume mechanical
supervision without changing its semantics.

## 3. P1 - Typed supervision plan

Goal: normalize manifest and workflow-binding supervision configuration before
dispatch.

Current code:

- `src/orchestration/atoms/types.rs`
- `src/orchestration/atoms/validate.rs`
- `src/workflow/schema.rs`
- `schema/atom.schema.json`
- `schema/workflow.schema.json`

Work:

- Add a runtime `SupervisionPlan` struct with classifier, advisor, recovery,
  trigger, tail-policy, and alert-dedup fields.
- Preserve `manifest.supervision.oracle` as compatibility vocabulary for
  classifier.
- Promote `AtomBinding.supervision_override` from arbitrary JSON to a typed
  override struct, or validate the JSON against the same typed shape at workflow
  compile time.
- Define merge order:
  1. atom manifest defaults
  2. workflow atom binding override
  3. caller/operator override, if any
- Fail closed on invalid modes, missing typed atom refs, malformed tail policy,
  negative budgets, or impossible advisor modes.

Tests:

- manifest defaults normalize to classifier/advisor disabled
- `oracle=default` resolves to configured default classifier atom
- binding override can disable classifier
- malformed override fails compile/dispatch validation
- alerting classification set is preserved

Deliverable: `run_atom_node` and `atom_invoke_value` can ask for one normalized
plan rather than interpreting raw manifest fields.

## 4. P2 - Attachment and polling primitives

Goal: let a supervision wrapper observe a primary invocation without granting
LLM tools or mutation rights.

Current code:

- `src/tools/atoms.rs`
- `src/workflow/engine.rs`
- `src/workflow/context.rs`
- `src/orchestration/mod.rs`
- `src/notes.rs`

Work:

- Define an attachment record:

```json
{
  "supervision_run_id": "...",
  "primary_invocation_id": "...",
  "primary_task_id": "...",
  "classifier_invocation_id": null,
  "advisor_invocation_id": null,
  "attempt": 1
}
```

- Store the attachment under wrapper-owned authority, not under the primary
  atom's composition budget.
- Add a deterministic polling primitive usable from workflow nodes or
  workflow-backed atoms. It should return:
  - invocation status
  - task status
  - mechanical supervision full snapshot
  - bounded recent provider events
  - bounded task notes
  - latest `bro_report`
  - bounded assistant tail
  - elapsed time and attempt metadata
- Enforce read-only authorization by supervision run lineage.
- Apply tail policy before the snapshot reaches any LLM.

Decision:

- Model this as a deterministic runtime primitive surfaced to workflows, not as
  an LLM-dispatched atom. The initial surface should be callable by
  workflow-backed atoms through a hook/op-style workflow operation. A future
  first-class node or deterministic atom wrapper may reuse the same primitive,
  but downstream classifier/advisor workflows should depend on the primitive's
  typed input/output contract rather than on provider tools.

Tests:

- authorized classifier can poll attached primary
- unrelated invocation cannot poll
- polling applies event/note/text byte limits
- full supervision snapshot is available even when task status uses green
  response optimization

Deliverable: classifier and advisor workflows can consume primary status by
snapshot, not by direct `bro_status` / `atom_status` tool calls.

## 5. P3a - Structured workflow exits

Goal: make workflow-backed atom results practical.

Current code:

- `src/workflow/schema.rs`
- `src/workflow/engine.rs`
- `src/tools/atoms.rs`

Work:

- Add structured workflow-backed atom exit output so parent callers can read
  classifier/advisor results without parsing concatenated node output.
- Record structured-exit events in the workflow audit trail.

Tests:

- workflow-backed atom exposes structured final output
- parent workflow can branch on structured child output

Deliverable: workflow-backed classifier and advisor atoms can return
machine-readable results.

## 6. P3b - Workflow sleep

Goal: make observer loops practical without busy polling.

Current code:

- `src/workflow/schema.rs`
- `src/workflow/engine.rs`
- `src/workflow/wait.rs`

Work:

- Add a workflow sleep/timer primitive. It may be a node field, a hook-op, or a
  pure-routing node type. Prefer sugar over existing `WaitSpec` timeout support
  with empty `any_of`; the synthetic `__timeout__` envelope is a control marker,
  not an external signal requirement.
- Ensure retry/visit counters and max-step protections apply to sleep loops.
- Record sleep events in the workflow audit trail.

Tests:

- back-edge loop with sleep does not hot-spin
- sleep respects cancellation
- cancellation during sleep resumes the workflow into a terminal cancelled
  state

Deliverable: workflow-backed classifier atoms can poll while the primary is
running without hot-spinning.

## 7. P4 - Classifier workflow-backed atom

Goal: implement the cheap observer as reusable data/artifact, not hard-coded
daemon behavior.

Depends on:

- P2 polling
- P3a structured workflow exits
- P3b workflow sleep

Inputs:

- attachment id or primary invocation id
- classifier policy from `SupervisionPlan`
- tail policy
- alerting classification set

Workflow shape:

```text
PollAttachedInvocation
  -> BuildClassifierSnapshot
  -> ClassifySnapshot
  -> GateClassifierVerdict
       nominal + primary_running -> Sleep -> PollAttachedInvocation
       nominal + primary_terminal -> ExitNoAlert
       alerting classification    -> ExitAlert
       classifier_failed          -> ExitClassifierFailed
```

Work:

- Author classifier atom manifest with strict output schema.
- Author workflow-backed classifier implementation.
- Keep classifier tool surface empty of `bro_*` and mutation-capable `bbox_*`.
- Implement classifier failure policy (`required=false` continues; required
  invokes advisor with classifier failure evidence).
- Store classifier findings as supervision-run evidence.

Tests:

- nominal running primary polls again
- terminal primary exits `no_alert`
- alerting classification exits `alert`
- schema violation exits `classifier_failed`
- classifier never mutates primary task

Deliverable: classifier-only supervised runs can observe and record findings
without steering.

## 8. P5 - Advisor workflow-backed atom

Goal: implement judgment as a reusable workflow-backed atom.

Depends on:

- P2 polling
- P3a structured workflow exits

Inputs:

- primary invocation/task snapshot
- classifier findings
- acceptance criteria
- attempt history
- allowed actions
- recovery policy

Workflow shape:

```text
PollPrimary
  -> CollectClassifierFindings
  -> BuildAdvisorCheckpoint
  -> Judge
  -> ValidateAction
  -> ExitAction
```

Work:

- Reuse/extract concepts from team-scoped advisor code in `src/tools/roster.rs`
  without binding advisor to team singletons.
- Author advisor atom manifest with strict action schema.
- Make advisor durable per supervised run.
- Include mechanical telemetry and classifier findings in the checkpoint.
- When `SupervisionPlan.classifier.mode == none`, `CollectClassifierFindings`
  returns an empty findings array and records classifier absence in the
  checkpoint so advisor-only and classifier-plus-advisor attempts share one
  shape.
- Ensure advisor cannot execute actions directly.

Tests:

- turn-end advisor accepts completed work that meets criteria
- advisor continues observing on false-positive classifier alert
- advisor emits `steer_primary` for incomplete profile-backed primary
- advisor schema violation fails closed
- advisor durability preserves attempt history

Deliverable: advisor-only and classifier-plus-advisor runs can produce typed
actions.

## 9. P6 - Typed action executor and recovery attempts

Goal: execute advisor actions through code-owned policy, not prompt behavior.

Depends on:

- P5 advisor workflow-backed atom

Current code:

- `src/tools/atoms.rs`
- `src/orchestration/mod.rs`
- `src/workflow/engine.rs`

Work:

- Add a typed action executor for:
  - `accept`
  - `continue_observing`
  - `steer_primary`
  - `cancel_and_retry`
  - `replace_primary`
  - `escalate_human`
  - `bail`
- Enforce lineage: only wrapper-owned advisor results can mutate the supervised
  primary.
- Enforce action compatibility:
  - `steer_primary` v1 only for profile-backed atoms resumable through
    `atom_resume`
  - workflow/deterministic/adapter steering is invalid until separate support
    exists
- Enforce retry budgets and attempt history.
- Deduplicate alert-source keys before invoking advisor.
- Scope cancellation to the primary task only.
- Persist action decisions and attempt history.

Tests:

- profile-backed `steer_primary` resumes with corrective prompt
- workflow-backed primary rejects `steer_primary`
- `cancel_and_retry` cancels only the primary task and starts a linked attempt
- `replace_primary` starts a replacement attempt and records lineage
- retry budget exhaustion routes to `escalate_human` or `bail`

Deliverable: supervised runs can re-enter or recover safely.

## 10. P7 - Runtime allocation and tiers

Goal: select classifier, advisor, and recovery lanes through allocator intent.

Depends on:

- `design/orchestration/supervision/runtime-allocation-tier-mapping.md`

Work:

- Add runtime intent fields to classifier/advisor/recovery plan sections.
- Derive `structured_output` for classifier/advisor atoms with strict schemas.
- Fail closed when the named tier ladder is missing for `at_least` or `bounded`.
- Respect provider capability tags. Today only Claude and Codex advertise
  `structured_output`.
- Record selection traces with supervision run id, attempt number, and action.
- Default recovery to exact-tier replacement unless escalation is explicitly
  configured.

Tests:

- classifier/advisor allocation rejects providers lacking `structured_output`
- missing tier ladder fails closed
- exact-tier replacement uses same tier
- escalation uses next eligible tier in configured ladder
- operator pins stay hard but do not bypass capability/health checks

Deliverable: recovery no longer hard-codes provider/model and remains
explainable.

## 11. P8 - Phase-decomposer integration

Goal: compose supervised atoms into the large-workflow decomposition path.

Depends on:

- P4 classifier atom for classifier-enabled runs
- P5 advisor atom for advisor-enabled runs
- P6 action executor for recovery

Work:

- Build a reusable supervised-atom subworkflow template.
- In phase decomposer, wrap fit-direct implementer and foreach implementers in
  that template.
- Export per-subunit advisor action, attempt count, final status, and evidence
  summary.
- Keep recomposition council as batch-level judgment; per-atom advisor remains
  subunit-level judgment.
- Use same-arc `Goto` for remediation epochs when council durability matters.

Tests:

- fit-direct path runs with advisor-only supervision
- foreach subunits run supervised wrappers in parallel
- one failed subunit produces a collected failure outcome
- council sees advisor action summaries and produces remediation packet
- same-arc remediation preserves council durable actor context

Deliverable: phase-decomposer can depend on supervision without owning its
internals.

## 12. Rollout order

Run P0 baseline verification before landing P1 so later phases have a stable
mechanical telemetry contract.

Recommended first shippable slice:

1. P1 typed plan normalization
2. P2 polling primitive
3. P3a structured exit
4. P5 advisor-only workflow-backed atom
5. P6 action executor with `accept`, `steer_primary`, `bail`

This slice enables turn-end advisor review for profile-backed atoms without the
classifier loop. Add P4 classifier next, then P7 recovery allocation and P8
phase-decomposer integration.

## 13. Cross-doc consistency checklist

Before landing each phase, check:

- `design/orchestration/supervision/supervision.md` still describes the same optional
  orchestration shape.
- `design/orchestration/supervision/supervision-classifier-cosession.md` still matches P2, P3b,
  and P4.
- `design/orchestration/supervision/supervision-turn-end-advisor.md` still matches P3a, P5, P6,
  and P7.
- `design/orchestration/supervision/supervision-test-plan.md` still covers the phase tests here.
- `design/orchestration/phase-decomposer/phase-decomposer-impl.md` still points at P-phase
  dependencies rather than stale S-only ranges.

## 14. Non-goals

- Do not move mechanical telemetry out of `SupervisionState`.
- Do not let classifier or advisor call mutation tools directly.
- Do not require classifier for every atom.
- Do not support workflow-backed primary steering in v1.
- Do not make tier names imply capabilities.
- Do not resurrect `acquire_drone` as a public dispatch tool.

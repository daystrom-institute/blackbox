# Classifier co-session as workflow-backed atom

Date: 2026-05-14
Status: partial design.

This document describes the semantic classifier layer for supervised atom
execution. It assumes the mechanical telemetry substrate described in
`design/archive/supervision-mechanical.md`.

Terminology: the current atom manifest schema calls this field
`manifest.supervision.oracle`. The design term is `classifier` because the
component is not authoritative. Until the schema grows a `classifier` alias,
`oracle` is compatibility vocabulary for the classifier atom.

## 1. Purpose

The classifier is a cheap semantic observer. It watches an attached primary atom
invocation and exits with a structured finding when it sees likely trouble that
mechanical counters cannot judge well.

It should not be a cheap bro instructed to remember to poll tools. Polling,
sleeping, loop control, bounded context construction, and exit routing should be
workflow/runtime mechanics. The LLM classifier should only classify a prepared
snapshot.

The classifier is not a fact checker. It cannot prove fabrication without
retrieval or domain-specific tools. It can only flag unsupportedness risk from
the snapshot it receives: a primary made claims without recent reads/searches,
the claims do not cite supplied evidence refs, the output contradicts
task-scoped notes, or the assistant tail appears to satisfy criteria that the
observed work did not touch. The advisor or a domain-specific reviewer owns
actual adjudication.

## 2. Proposed shape

The classifier co-session is a workflow-backed atom:

```text
PollAttachedInvocation
  -> BuildClassifierSnapshot
  -> ClassifySnapshot
  -> GateClassifierVerdict
       nominal + primary_running -> Sleep -> PollAttachedInvocation
       nominal + primary_terminal -> ExitNoAlert
       concern                   -> ExitAlert
       classifier_failed          -> ExitClassifierFailed
```

The workflow runs for the duration of the attached primary invocation, or until
it emits an alert, hits a polling/time budget, or the primary reaches a terminal
state.

`Sleep` is a needed primitive, not current workflow engine behavior. See
S4-sleep-primitive in `design/partial/supervision-impl.md`, implemented in
phase P3b of `design/partial/supervision-phased-implementation.md`.

## 3. Responsibilities

### Workflow/runtime responsibilities

- Poll the attached primary invocation/task.
- Include current mechanical supervision telemetry.
- Include bounded recent events, reports, notes, and terminal output snippets.
- Enforce tail size and redaction policy.
- Sleep between polls.
- Apply max-poll and max-elapsed budgets.
- Validate classifier output schema.
- Branch on structured verdicts.
- Exit with a machine-readable result.

### Classifier LLM responsibilities

- Classify the snapshot.
- Provide a confidence score.
- Cite the short evidence fields it relied on.
- Avoid control actions.

The classifier does not call `bro_status`, `atom_status`, `bro_cancel`,
`atom_resume`, or any file-editing tool. It does not need those tools in its
surface.

## 4. Snapshot contract

The classifier input should be a bounded envelope:

```json
{
  "primary": {
    "invocation_id": "invocation-id",
    "atom_ref": "atom:name@v1",
    "implementation_kind": "profile",
    "task_id": "task-id",
    "status": "running",
    "elapsed_ms": 120000
  },
  "contract": {
    "acceptance_criteria": ["criterion"],
    "declared_effects": {},
    "current_attempt": 1,
    "max_attempts": 3
  },
  "mechanical_supervision": {
    "ok": false,
    "alerts": []
  },
  "recent": {
    "events": [],
    "notes": [],
    "report": null,
    "assistant_tail": "bounded text"
  }
}
```

The classifier output should be narrow:

```json
{
  "classification": "nominal",
  "concern_kind": null,
  "confidence": 0.72,
  "reason": "short reason",
  "evidence_refs": ["mechanical_supervision.alerts[0]"]
}
```

Recommended classification lattice:

- `nominal`
- `maybe_stuck`
- `maybe_scope_drift`
- `maybe_unsupported_claims`
- `maybe_tool_misuse`
- `needs_advisor`
- `classifier_failed`

The exact lattice can be packet/workflow data. The runtime only needs to know
which classifications are alerting.

## 5. Needed primitives

Current workflows have loops, branches, atom nodes, and workflow-backed atoms,
but need a few additional primitives for a clean classifier loop:

- `poll_atom_invocation` or `poll_task_status`: deterministic node returning
  bounded status plus `SupervisionState`.
- `build_classifier_snapshot`: deterministic shaping/redaction node, or a
  standard workflow helper.
- `sleep` or timer wait: prevents hot back-edge loops.
- structured workflow-backed atom exit output: parent wrapper can distinguish
  `no_alert`, `alert`, `primary_done`, and `classifier_failed`.
- observation grant: classifier can read the attached invocation/task and notes
  but cannot mutate primary state.
- tail policy: data-owned limits for recent event count, assistant text bytes,
  notes, and reports.

These are reusable workflow/runtime primitives, not classifier-specific daemon
sidecar code.

The canonical data path is snapshot-passing, not tool access. The wrapper or
workflow runtime polls status and notes through deterministic primitives,
constructs the snapshot, and passes it to the classifier node. The classifier
does not receive `bro_*` or mutation-capable `bbox_*` tools.

## 6. Triggering

Classifier mode is policy data. Useful modes:

- `none`
- `on_mechanical_alert`
- `cadence`
- `cadence_or_alert`
- `turn_end_only`

For long-running profile-backed atoms, `cadence_or_alert` is the most useful
mode. For short tasks, `turn_end_only` or `none` is cheaper.

The alerting lattice belongs in data. `SupervisionPlan.alerting_classifications`
or the classifier workflow's gate packet decides which classifier outputs exit
as alerts. The classifier prompt should not own that routing policy.

## 7. Relationship to advisor

A classifier alert summons the advisor. It does not cancel the primary directly.
The advisor receives classifier findings as evidence and may decide:

- the alert is a false positive
- the primary should be steered
- the primary should be cancelled and retried
- a replacement/recovery primary should be dispatched
- the user must decide

## 8. Failure behavior

Classifier failure must not automatically fail the primary. The wrapper should
record the failure and continue according to policy:

- `classifier_required=false`: continue primary, maybe invoke turn-end advisor
- `classifier_required=true`: invoke advisor with classifier failure evidence
- repeated classifier failures: disable classifier for the run and record a
  `surprise` or equivalent audit note

`classifier_required` is normalized supervision-plan data, not an LLM prompt
convention.

## 9. Runtime tier

Classifier runtime should usually be cheap:

- tier: `economy` or a custom low-cost classifier tier
- required capabilities: `structured_output`
- selection policy: low latency or availability
- durable: usually false

The classifier does not need continuity if every poll receives a complete
bounded snapshot. Stateful classifier sessions are optional and should be an
explicit cost/quality choice, not the default.

Current provider capability tags matter. In current code only Claude and Codex
advertise `structured_output`; GLM, DeepSeek, Inception, Gemini, and Vibe are
not eligible for a classifier atom that hard-requires structured output unless
their capability tags or the atom's contract change.

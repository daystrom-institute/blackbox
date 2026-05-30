---
title: "Supervision - optional atom orchestration"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - supervision
date: 2026-05-14
status: "partial design, atom-era revision"
brief: "Splits supervised atom execution into mechanical signals, optional classifier observation, and advisor recovery policy."
---

# Supervision - optional atom orchestration

This document replaces the pre-atom supervision proposal. The old proposal
mixed three different layers:

1. mechanical supervision signals
2. semantic classifier co-session
3. advisor judgment and recovery policy

Those layers should stay separate. Mechanical supervision is code-owned
telemetry. Classifier and advisor are optional orchestration around a primary
atom invocation.

Related docs:

- `design/orchestration/supervision/supervision-mechanical.md` - implemented mechanical
  supervision substrate.
- `design/orchestration/supervision/supervision-classifier-cosession.md` - workflow-backed
  classifier co-session.
- `design/orchestration/supervision/supervision-turn-end-advisor.md` - turn-end and alert-driven
  advisor loop.
- `design/orchestration/supervision/supervision-test-plan.md` - test skeleton.
- `design/orchestration/supervision/supervision-phased-implementation.md` - phased build plan.
- `design/orchestration/supervision/runtime-allocation-tier-mapping.md` - tier keys, ladders,
  runtime allocation, and recovery lane selection.
- `design/orchestration/supervision/acquire-drone.md` - superseded pool/probe donor material.

## 1. Intent

Supervision is an optional wrapper around atom execution. Not every atom,
brofile, workflow actor, or dispatch needs it. The runtime must support:

- no classifier and no advisor
- classifier only
- advisor only
- classifier plus advisor
- advisor only at turn end
- advisor summoned early by classifier or mechanical signals

The primary unit remains the atom. Supervision does not change the primary
atom's public contract; it observes, judges, and may re-enter or replace the
primary according to a bounded policy.

## 2. Three logical nodes

### Primary atom

The primary atom does the work: code execution, research, documentation
maintenance, refactor planning, or whatever capability the workflow requested.

The primary atom may be re-entrant when the underlying implementation supports
resume. In current code, profile-backed atoms are resumable through
`atom_resume`; workflow-backed atoms are observed through workflow state rather
than profile-session resume. Deterministic and adapter-backed atoms normally do
not need live classifier supervision because they complete synchronously or hide
their own lifecycle behind the adapter.

### Classifier atom

The classifier is a cheap semantic observer. It is not mechanical supervision,
and it is not a controller.

Its purpose is to identify behavior that simple counters cannot judge well:

- possible scope drift
- possible unsupportedness: claims not backed by the provided evidence,
  recent tool activity, task notes, or accepted sources in the bounded snapshot
- possible struggle despite non-terminal progress
- suspicious tool-use pattern where the mechanical signal is ambiguous
- missing acceptance-criteria attention before the primary claims done

The classifier does not cancel, steer, resume, replace the primary, or prove
truth. It exits with a structured finding. A non-nominal finding can summon the
advisor early.

### Advisor atom

The advisor is the judgment layer. It evaluates the primary's work at turn end
and, optionally, on classifier/mechanical alert. It scores work against the atom
contract and task acceptance criteria, then emits a bounded action:

- accept
- continue observing
- steer the primary with corrective instructions
- cancel and retry
- replace the primary bro/persona/provider
- escalate to a human
- bail with a documented failure

The advisor can decide that a classifier alert was a false positive. It can also
decide that the primary should be replaced by a stronger or different runtime
lane using the tier ladder from `runtime-allocation-tier-mapping.md`.

## 3. Current code anchors

These are the real seams that should shape the design.

Implemented today:

- `src/orchestration/supervision.rs` implements task-local mechanical
  telemetry: loop, compaction, token-burn alerts, a neutral idle notice (no
  longer a stall alert), recent hashes, and response snapshots.
- `src/orchestration/mod.rs` feeds `SupervisionState` from streaming provider
  events and bulk parsed provider output, then surfaces `supervision` in task
  result/status/timeout JSON.
- `schema/atom.schema.json` and
  `src/orchestration/atoms/types.rs` define atom manifest supervision fields:
  `oracle` and `advisor`, both defaulting to `none`.
- `src/orchestration/atoms/validate.rs` validates reserved supervision values
  and typed atom refs.
- `src/workflow/schema.rs` defines `AtomBinding.supervision_override`.
- `src/tools/atoms.rs` implements profile-backed and workflow-backed atom
  invocation, status, and resume.
- `src/workflow/engine.rs` can invoke atom nodes, run back-edge loops, branch
  on gate verdicts, fork fire-and-forget actor work, and join in-flight work.
- `src/tools/roster.rs` has real team-scoped advisor prompt/checkpoint/packet
  code, but it is not a generic atom supervision loop.

Not implemented today:

- `manifest.supervision` is not consumed during atom invocation.
- `AtomBinding.supervision_override` is not consumed by `run_atom_node`.
- There is no generic supervision wrapper that starts primary plus classifier
  plus advisor.
- There is no workflow sleep/timer primitive for clean polling loops.
- There is no deterministic workflow node that polls an attached
  invocation/task and returns a bounded status envelope.
- There is no typed action executor for advisor decisions such as steer, cancel,
  retry, replace, or human escalation.

## 4. Code versus data boundary

Code should own the runtime mechanics:

- normalize atom-level and binding-level supervision configuration
- start the primary invocation
- grant read-only observation capability to classifier/advisor invocations
- poll task/invocation status
- sleep between classifier polls
- collect mechanical telemetry, notes, reports, and bounded event tails
- invoke classifier and advisor atoms
- validate structured outputs
- execute advisor actions through typed primitives
- persist the supervision audit trail

Data should own policy:

- whether classifier and advisor are enabled
- classifier atom ref and advisor atom ref
- polling cadence and trigger rules
- maximum polls, elapsed time, and retry budgets
- classifier and advisor output schemas
- verdict/action lattice
- acceptance criteria and steering prompt templates
- recovery tier ladder, tier mode, pool, and selection policy

The runtime should normalize compatibility fields into a richer internal plan.
The existing manifest shape:

```json
{
  "supervision": {
    "oracle": "default",
    "advisor": "on_alert"
  }
}
```

should lower to a structured supervision plan. New docs should prefer the term
`classifier` over `oracle`; `oracle` is retained only as compatibility
vocabulary in the existing schema.

In current code this requires tightening one loose surface:
`AtomBinding.supervision_override` is untyped JSON. The atom-era runtime should
promote it to a typed override struct, or validate it against the same schema
used by manifests before dispatch.

Conceptual normalized shape:

```json
{
  "classifier": {
    "mode": "none|on_mechanical_alert|cadence|cadence_or_alert|turn_end_only",
    "atom_ref": "atom:behavior-classifier@v1",
    "cadence_ms": 10000,
    "required": false,
    "alerting_classifications": [
      "maybe_stuck",
      "maybe_scope_drift",
      "maybe_unsupported_claims",
      "maybe_tool_misuse",
      "needs_advisor"
    ],
    "tail_policy": {
      "events": 20,
      "notes": 20,
      "assistant_bytes": 4000
    }
  },
  "advisor": {
    "mode": "none|on_alert|always",
    "atom_ref": "atom:turn-end-advisor@v1",
    "durable": true
  },
  "recovery": {
    "max_attempts": 3,
    "tier_ladder": "coding-quality",
    "tier_mode": "exact"
  }
}
```

Capability eligibility still applies to this plan. A classifier that requires
`structured_output` is currently Claude/Codex-only unless provider capability
tags or the classifier contract change.

Mechanical alerts and classifier alerts should dedupe before advisor
invocation. A mechanical alert is evidence that can trigger an advisor directly
when the classifier is disabled; when the classifier is enabled, the wrapper can
either wake the classifier poll immediately or invoke the advisor directly by
policy, but it must record an alert-source key so the same condition does not
double-summon the advisor.

## 5. Proposed orchestration shape

The preferred shape is a wrapper-owned supervision run:

```text
supervised atom invocation
  start primary atom
  maybe start classifier workflow-backed atom
  wait for primary turn end or classifier alert
  maybe invoke advisor workflow-backed atom
  apply advisor action
  repeat until accepted, exhausted, escalated, or bailed
```

The primary atom should not invoke its own classifier. Supervision is
orchestration around the primary, not behavior initiated by the primary. That
avoids composition-budget ambiguity and lets the wrapper grant observation
rights without granting mutation rights.

The wrapper is also the authority boundary. It owns the supervised run and
delegates observe-only rights to classifier work and judge/action-proposal
rights to advisor work. The final action executor validates that lineage before
mutating primary state.

## 6. Runtime allocation and tiers

Classifier, advisor, and recovery primary attempts should use runtime
allocation intent instead of hard-coded provider/model choices.

Typical defaults:

- classifier: cheap/economy tier, structured output, low latency
- advisor: standard or premium tier, structured output, durable if multi-round
- replacement primary: same tier first for persona/provider replacement, then
  higher tier on recovery escalation

The advisor should not hard-code model names. It should request recovery through
a named tier ladder such as `coding-quality`, using the allocator semantics in
`design/orchestration/supervision/runtime-allocation-tier-mapping.md`.

## 7. Design position

Classifier co-session should be reusable workflow-backed atom machinery, not a
daemon-special sidecar and not a prompt that remembers to call `bro_status`.

The workflow mechanically polls and loops. The LLM classifies only the prepared
snapshot. The advisor judges only bounded evidence and emits a typed action.

Mechanical supervision remains below this as telemetry. It can trigger the
classifier/advisor, but it is not the classifier and it is not the advisor.

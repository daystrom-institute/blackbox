# Turn-end advisor and recovery loop

Date: 2026-05-14
Status: partial design.

The advisor is the judgment layer for supervised atom execution. It can run at
primary turn end, on classifier alert, or both.

## 1. Purpose

The advisor evaluates work against:

- the primary atom contract
- task acceptance criteria
- declared effects and observed effects
- mechanical supervision telemetry
- classifier findings
- primary output and notes
- attempt count and recovery policy

It emits a typed action. The runtime executes the action if allowed.

## 2. Preferred shape

Advisor should also be a workflow-backed atom:

```text
PollPrimary
  -> CollectClassifierFindings
  -> BuildAdvisorCheckpoint
  -> Judge
  -> ValidateAction
  -> ExitAction
```

The LLM judge sees a structured checkpoint. It does not directly call
`bro_cancel`, `atom_resume`, or dispatch replacement work. The runtime executes
only validated actions.

## 3. Checkpoint contract

Advisor input:

```json
{
  "primary": {
    "invocation_id": "invocation-id",
    "atom_ref": "atom:name@v1",
    "implementation_kind": "profile",
    "status": "completed",
    "summary": "bounded output",
    "output_shape": {}
  },
  "acceptance": {
    "criteria": ["criterion"],
    "known_failures": []
  },
  "supervision": {
    "mechanical": {},
    "classifier_findings": []
  },
  "attempts": {
    "current": 1,
    "max": 3,
    "history": []
  },
  "recovery": {
    "tier_ladder": "coding-quality",
    "allowed_actions": []
  }
}
```

Advisor output:

```json
{
  "action": "accept",
  "reason": "short rationale",
  "confidence": 0.83,
  "steering_prompt": null,
  "replacement": null,
  "human_prompt": null,
  "bail_summary": null
}
```

Recommended action lattice:

- `accept`
- `continue_observing`
- `steer_primary`
- `cancel_and_retry`
- `replace_primary`
- `escalate_human`
- `bail`

## 4. Action semantics

### accept

The advisor judges that acceptance criteria are met. The wrapper exits
successfully.

### continue_observing

The advisor was invoked early and does not want intervention. The primary keeps
running and the classifier may continue polling.

### steer_primary

The runtime sends a corrective prompt back to the same primary invocation when
the primary implementation is profile-backed, resumable through `atom_resume`,
and the attempt budget allows it.

For deterministic, workflow-backed, adapter-backed, or otherwise non-resumable
implementation kinds, this action is invalid and should be rejected by
`ValidateAction` before runtime execution. Future workflow-backed steering can
be added as a separate action once workflow continuations support corrective
input.

### cancel_and_retry

The runtime cancels or waits out the current primary attempt according to
policy, then starts a new attempt with the same primary atom and possibly a
different runtime lane.

### replace_primary

The runtime starts a new primary atom/persona/provider selected by recovery
policy. Replacement may be same tier/different provider or higher tier depending
on advisor output and configured tier ladder.

### escalate_human

The wrapper halts in a blocked state with a concrete question for the operator.
This should be the second-last resort: use it when policy cannot choose safely.

### bail

The wrapper exits with documented failure after attempts are exhausted or the
advisor determines the task is untenable without further operator input.

## 5. Existing advisor code and reuse

`src/tools/roster.rs` contains team-scoped advisor machinery:

- advisor init prompt
- checkpoint builder
- packet pre-classification
- durable advisor resume
- verdict response handling

That code is real but scoped to teams. The atom-era advisor should extract or
reuse the checkpoint/prompt/packet concepts without binding the design to team
singletons.

Workflow actor kind `advisor` exists as convention, but current workflow docs
also say persona/role is mostly brofile lens plus prompt. The new advisor should
be defined by its atom/workflow contract, not by a special engine actor type.

## 6. Runtime action executor

The final control step should be code-owned. A typed action executor validates:

- the advisor is linked to the supervised run through wrapper-owned delegation
- the action is allowed by policy
- the primary implementation supports the requested action
- retry/attempt budgets are not exhausted
- replacement/recovery tier request is valid against the named tier ladder
- cancellation is scoped to the primary task only

The advisor's prose reason is audit data, not control flow.

## 7. Recovery and tiers

Recovery should use the allocator model from
`design/proposed/runtime-allocation-tier-mapping.md`.

Examples:

- same-tier replacement: `tier_mode=exact`, same target tier, different
  provider/persona preference
- escalation: `tier_mode=at_least`, target next tier in `coding-quality`
  ladder
- bounded retry: `tier_mode=bounded`, min current tier, max premium/frontier

Operator pins remain authority but do not override capability, health,
account, or safety constraints.

If a recovery action uses `at_least` or `bounded`, the named tier ladder must
exist at allocation time. Per the runtime allocation design, missing ladders
fail closed. Operators enabling advisor recovery escalation must configure the
ladder first; otherwise default recovery should stay at `tier_mode=exact`.

Capability eligibility still applies to replacement lanes. In current code only
Claude and Codex advertise `structured_output`; replacement or recovery attempts
that require structured output cannot land on GLM, DeepSeek, Inception, Gemini,
or Vibe until their provider capability tags or runtime support change.

## 8. Advisor durability

Advisor should usually be durable for one supervised run. It may be invoked
multiple times:

- early classifier alert
- primary claims done
- after steering turn
- after replacement attempt
- after retries exhausted

Durability lets the advisor remember prior attempts and avoid repeating the
same bad recovery recommendation. The durable state is scoped to the supervised
run, not global.

## 9. Human escalation

Human escalation should carry:

- primary atom ref and invocation id
- current attempt count and max attempts
- advisor reason
- classifier findings
- mechanical alerts
- proposed options
- exact question requiring operator authority

When the user responds, the wrapper resumes with that decision rather than
starting a new unrelated run.

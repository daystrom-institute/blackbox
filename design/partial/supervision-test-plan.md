# Supervision test plan skeleton

Date: 2026-05-14
Status: skeleton.

This skeleton covers the atom-era supervision model:

- implemented mechanical telemetry
- workflow-backed classifier co-session
- turn-end advisor
- typed recovery actions
- runtime allocation tier integration

`design/archive/supervision-phased-implementation.md` is the active execution
plan and lists tests at each phase boundary. This file is the broader coverage
checklist to keep those phase tests complete.

## 1. Mechanical telemetry regression

Existing unit tests in `src/orchestration/supervision.rs` should remain the
baseline.

Additional checks:

- task status includes green `supervision.ok=true` for nominal runs
- task status includes full supervision snapshot when alert thresholds are hit
- bulk-output providers update supervision event count and usage
- response-optimized snapshots do not remove machine-readable full snapshot
  access for internal workflow polling primitives

## 2. Atom manifest and binding normalization

Tests:

- manifest defaults normalize to classifier/advisor disabled
- `oracle=default` normalizes to configured default classifier atom
- `advisor=on_alert` normalizes to alert-driven advisor mode
- typed classifier/advisor atom refs are preserved
- binding `supervision_override` can disable manifest classifier
- binding `supervision_override` can add advisor to an otherwise unsupervised
  atom
- malformed binding `supervision_override` is rejected before dispatch
- invalid override fails validation before dispatch

## 3. Polling primitive

Tests for `poll_atom_invocation` / `poll_task_status`:

- returns bounded status for running profile-backed atom
- returns terminal status for completed atom
- includes mechanical supervision snapshot
- applies event-tail and note-tail limits
- refuses unauthorized reads
- cannot mutate the primary invocation

## 4. Classifier workflow-backed atom

Fixture workflows:

- nominal primary stays running until max one poll then exits no-alert in a
  deterministic test mode
- mechanical red alert produces classifier alert exit
- classifier concern output exits alert and does not cancel primary
- classifier schema violation exits classifier_failed
- classifier max polls exits no-alert or budget-exhausted according to policy
- sleep/timer prevents busy-loop execution

## 5. Advisor workflow-backed atom

Tests:

- completed primary with acceptance-satisfying output emits `accept`
- incomplete output emits `steer_primary`
- early classifier false positive emits `continue_observing`
- non-resumable primary plus `steer_primary` is rejected by action validation
- exhausted attempts emit `bail` or `escalate_human`
- advisor receives classifier findings and mechanical alerts in checkpoint
- advisor output schema violation fails closed

## 6. Action executor

Tests:

- `steer_primary` resumes profile-backed primary with corrective prompt
- `steer_primary` refuses deterministic/workflow-backed non-resume handles
- `cancel_and_retry` cancels only the primary task
- `replace_primary` creates a new attempt and links attempt history
- retry budget is enforced
- human escalation produces blocked state with exact prompt
- bail records structured failure summary

## 7. Runtime allocation tiers

Tests:

- classifier default requests cheap tier plus structured output
- advisor default requests standard/premium tier plus structured output
- replacement primary uses same-tier replacement before escalation when policy
  says so
- escalation uses named tier ladder and fails closed without a valid ladder
- operator pins remain hard constraints but still validate capabilities/health
- selection trace records supervision caller identity and attempt number

## 8. End-to-end workflows

Scenarios:

- unsupervised atom behavior remains unchanged
- classifier-only run records classifier findings but never steers/cancels
- advisor-only run evaluates turn end and accepts
- classifier alert summons advisor early; advisor continues observing
- classifier alert summons advisor early; advisor cancels and retries
- turn-end advisor steers primary once, then accepts second turn
- replacement primary succeeds after first primary fails
- attempts exhausted leads to human escalation, then user decision resumes run
- attempts exhausted with no safe recovery bails with documentation

## 9. Doc-lint / review consistency

Review assertions that should become doc-lint where practical:

- `supervision.md` does not imply mechanical telemetry cancels or judges.
- `supervision-classifier-cosession.md` does not require LLM tool-polling and
  does not claim the classifier can prove fabrication without retrieval.
- `supervision-turn-end-advisor.md` keeps action execution code-owned.
- `runtime-allocation-tier-mapping.md` remains the source of tier semantics.
- `acquire-drone.md` remains superseded and only donor material.
- phase decomposer docs refer to supervised subworkflow composition without
  owning classifier/advisor internals.

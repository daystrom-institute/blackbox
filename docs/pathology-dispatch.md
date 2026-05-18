# Architecture Pathology Dispatch

Architecture pathology is the workflow lane for turning Java architecture
smells into a reviewed correction plan. It is diagnostic only while it runs: it
does not edit source during diagnosis. Its output is meant to be automation
ready for PD-dispatch once the operator chooses to launch implementation.

The workflow currently ships one language-specific lane:

- `arch-pathology-java` surveys a Java project, dispatches the justified subset
  of Java architecture pathology atoms, reviews their claims on a whiteboard,
  and writes a correction plan under `design/refactor/plans/`.

Do not use pathology as a larger lint run. If SAST, ArchUnit, a clone detector,
or a metric threshold can make the diagnosis by itself, encode that static rule
directly. Pathology is for semantic architecture judgments: role mismatch,
responsibility ownership, framework lifecycle contract, context capture, and
history-backed pressure.

## Install Artifacts

Install or refresh the Java pathology artifacts before dispatching by workflow
id:

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-role-behavior-coherence.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-responsibility-bleed.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-conceptual-duplicate-discovery.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-anemic-data-remote-behavior.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-scoped-context-capture.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-framework-contract-violation.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-test-implied-architecture.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-architecture-transcript-anchored-pressure.json")

bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/java-architecture-pathologist.json")

bbox_artifact_install(kind="workflow", source="system-defaults/workflows/refactor/arch-pathology-java.json")
```

If a dispatch reports an unknown atom, brofile, or workflow, install the named
artifact and rerun. If it reports an unknown hook operation, restart the daemon
after installing the current `blackboxd`; pathology uses native workflow hooks
for atom-request normalization and plan writing.

## Invocation

Use `/orchestrate/by-id` or MCP `bro_orchestrate_run` with `workflow_id =
"arch-pathology-java"`. `bro orchestrate run <file>` is useful for dry-run
validation, but real runs should use the installed workflow id so atom and
brofile references resolve through the production artifact catalog.

Required initial vars:

```json
{
  "project_dir": "/repo",
  "scope_filter": ".",
  "target_context_window": 10000,
  "operator_hints": [
    "whole-project architecture pathology pass",
    "pay attention to UI layer boundaries, DI scopes, and report generation"
  ],
  "target_loci": [],
  "layer_model_path": "",
  "whole_project_mode": true
}
```

`scope_filter` may be a package, directory, file, or `"."` for a broad pass.
Use `target_loci` when you already know the suspicious files. Use
`operator_hints` for prior pain, transcript anchors, framework concerns, or
known architectural boundaries. Use `layer_model_path` when the project already
has a written layer contract; otherwise leave it empty and the survey will infer
boundaries conservatively.

Example HTTP dispatch:

```bash
PORT="${BBOX_PORT:-7264}"
PROJECT="/repo"

jq -n \
  --arg project_dir "$PROJECT" \
  '{
    workflow_id: "arch-pathology-java",
    project_dir: $project_dir,
    max_steps: 80,
    await_completion: false,
    initial_vars: {
      project_dir: $project_dir,
      scope_filter: ".",
      target_context_window: 10000,
      operator_hints: [
        "whole-project architecture pathology pass",
        "look for presentation classes doing business, persistence, transport, or scheduler work"
      ],
      target_loci: [],
      layer_model_path: "",
      whole_project_mode: true
    }
  }' |
curl -sS -H 'content-type: application/json' \
  -d @- "http://127.0.0.1:${PORT}/orchestrate/by-id" | jq .
```

Poll with the returned ids:

```bash
bro orchestrate status <arcId-or-threadId>
bro_status(task_id="<taskId>")
bro_wait(task_id="<taskId>")
bro_arc_status(arc_id="<arcId>")
```

## What The Workflow Does

1. `Setup` records the baseline commit, opens a whiteboard, and registers
   `pathologist` as facilitator.
2. `Survey` does cheap grounding first: code symbols, refs, refactor status,
   dependency analysis for candidate classes, transcript search, and notes. It
   selects only the atom requests justified by evidence.
3. `FocusedAtoms` dispatches the selected Java architecture atoms in parallel
   and collects their structured diagnosis results. A broad run may still
   dispatch fewer than all atoms; that is expected.
4. `Review` merges overlapping claims, rejects SAST-shaped or weak findings,
   advances the whiteboard from blind to read/debate/resolve, and retains only
   architecture-level diagnoses.
5. `SynthesizePlan` writes strict JSON for a correction plan: diagnosis summary,
   evidence, remediation slices, acceptance criteria, and deferred/rejected
   candidates.
6. `WritePlan` writes markdown to
   `<project>/design/refactor/plans/<slug>.md`.

The generated plan is proposed, not executed by the pathology run itself. It is
meant to be reviewed and then handed to PD-dispatch for automated
implementation.

## Java Pathology Atoms

The survey may choose from these atoms:

- `java-architecture-role-behavior-coherence`: class role versus actual
  behavior.
- `java-architecture-responsibility-bleed`: one conceptual responsibility
  scattered or centralized without a canonical owner.
- `java-architecture-conceptual-duplicate-discovery`: different Java units
  solving the same architectural problem under different names or paths.
- `java-architecture-anemic-data-remote-behavior`: behavior living on the wrong
  side of a data/behavior split.
- `java-architecture-scoped-context-capture`: short-lived context stored into
  longer-lived state.
- `java-architecture-framework-contract-violation`: framework API use that
  violates caller role or lifecycle.
- `java-architecture-test-implied-architecture`: tests revealing an intended
  architecture not present in production code.
- `java-architecture-transcript-anchored-pressure`: prior operator/agent
  history corroborating current code pressure.

## Output

Successful runs write a plan with frontmatter like:

```yaml
kind: correction-plan
lifecycle: proposed
corpus: project-refactor
generated_by: arch-pathology
baseline_commit: <sha>
```

The body contains:

- `Diagnosis Summary`
- `Evidence`
- `Remediation Plan`
- `Acceptance Criteria`
- `Deferred`
- `Dispatch Payload`

Review the plan before implementation. Tighten acceptance criteria, delete weak
slices, and reorder remediation if the operator knows project constraints the
pathologist could not infer.

## Remediation Handoff

Use the generated plan as the phase document for PD-dispatch:

```json
{
  "workflow_id": "phase-decompose-main-edit",
  "project_dir": "/repo",
  "initial_vars": {
    "phase_doc_path": "design/refactor/plans/<slug>.md",
    "phase_doc_text": "<full correction plan text>",
    "project_dir": "/repo",
    "target_context_window": 10000,
    "epoch": 0,
    "max_epochs": 3,
    "acceptance_criteria": [
      {
        "id": "AP-1",
        "criterion_text": "Layer contract is documented and enforced by ArchUnit."
      }
    ]
  }
}
```

The `Dispatch Payload` section in the generated plan is the automation handoff.
Review it, tighten acceptance criteria if needed, then run it through
PD-dispatch. See [Phase-Decomposer Dispatch](pd-dispatch.md) for the
implementation lane.

## Operator Rules

- Start from an indexed project. If the survey reports weak code index coverage,
  reindex/reembed before treating a broad negative result as meaningful.
- Prefer `scope_filter` and `target_loci` when you already know the pressure
  area. Whole-project mode is useful, but expensive and noisy.
- Treat "fewer atoms dispatched" as normal. The survey should not run all atoms
  just because they exist.
- Do not accept SAST-shaped findings as pathology. Convert them into static
  rules, ArchUnit tests, lints, or clone-detection follow-ups.
- Do not edit code in pathology. The output is a correction plan; implementation
  belongs to PD-dispatch or an explicitly scoped manual edit.
- Do not frame remediation as human-only. Pathology cannot silently execute it,
  but PD-dispatch can implement it after the operator launches the reviewed
  plan.
- Keep whiteboard claims concise and evidence-backed. The board is for surviving
  diagnosis pressure, not raw search output.
- Commit remains operator-owned. Pathology may create a plan file in the target
  project, but committing that plan is a separate explicit step.

## Example Result Shape

A broad run should produce architecture-level findings such as:

- Role-boundary findings where presentation classes own transport, scheduler,
  persistence, or cross-domain event handling concerns.
- Responsibility-bleed findings where one dispatcher or coordinator owns too
  many unrelated domain decisions.
- DI-seam findings where scoped objects acquire dependencies through service
  locators, static containers, or lifecycle-unsafe APIs.
- Conceptual-duplicate findings where sibling classes are thin variants of the
  same architectural behavior.

The run wrote:

```text
design/refactor/plans/<slug>.md
```

The usual remediation path is to declare and enforce the relevant architecture
contract first, then extract or relocate the worst offenders, clean up lifecycle
or DI seams, and collapse confirmed duplicate clusters.

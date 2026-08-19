# Phase-Decomposer Dispatch

PD-dispatch is the workflow lane for turning a large implementation document
into bounded, evidence-sized work. It should be used when the work is too large
or too context-heavy for one provider session to safely hold in its head.

The workflow is intentionally split into two lanes:

- `phase-decompose-main` is the no-edit smoke and validation lane. It proves
  decomposition, foreach dispatch, supervision, and recomposition without
  allowing file edits.
- `phase-decompose-main-edit` is the implementation lane. It uses the same
  discovery, ensemble decomposition, and recomposition machinery, but routes
  direct and foreach implementation through `phase-decompose-supervised-impl-edit`.

Do not replace the no-edit lane with the edit lane. The no-edit lane is the
regression/smoke surface; the edit lane is the operator-facing implementation
surface.

## Install Artifacts

Install or refresh the shared phase-decompose dependencies before dispatching by
workflow id:

```text
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/phase-decompose/triage.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/phase-decompose/dag-structure.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/phase-decompose/whiteboard-participation.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/phase-decompose/recompose-verdict.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/phase-decompose/epoch-ceiling.json")

bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/inlet.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/corpus-pathfinder.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/decomposer-facilitator.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/decomposer-architecture.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/decomposer-implementation.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/decomposer-risk.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/acceptance-advisor.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/recompose-facilitator.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/recompose-integration.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/recompose-acceptance.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/implementer.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/phase-decompose/edit-implementer.json")

bbox_artifact_install(kind="agent", source="system-defaults/agents/corpus-pathfinder.json")

bbox_artifact_install(kind="team", source="system-defaults/phase-decompose/teamplates/decomposer-panel.json")
bbox_artifact_install(kind="team", source="system-defaults/phase-decompose/teamplates/recompose-council.json")

bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/discovery.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/ensemble-decompose.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/supervised-impl.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/supervised-impl-edit.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/recompose.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/main.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/phase-decompose/main-edit.json")
```

If a dispatch reports `subworkflow_ref ... not in registry`, install the named
workflow artifact and rerun. If a dispatch reports an unknown packet domain or
brofile, install the corresponding packet or brofile first.

## Invocation

Use `/orchestrate/by-id` or MCP `bro_orchestrate_run` with `initial_vars`.
`bro orchestrate run <file>` validates a raw workflow file but does not seed the
phase document variables.

For read-only validation:

```text
workflow_id = "phase-decompose-main"
```

For implementation:

```text
workflow_id = "phase-decompose-main-edit"
```

Required initial vars:

```json
{
  "phase_doc_path": "design/operations/config-artifacts/ops-artifact-bundles-and-doctor-impl.md",
  "phase_doc_text": "...full doc text...",
  "project_dir": "/home/invidious/repos/transcript-search",
  "target_context_window": 10000,
  "epoch": 0,
  "max_epochs": 3,
  "acceptance_criteria": [
    {
      "id": "AB-P0",
      "criterion_text": "Baseline current behavior and source anchors before implementation."
    }
  ]
}
```

`acceptance_criteria` should be small, explicit, and stable. Prefer one
criterion per phase or acceptance gate in the implementation document. The
decomposer may repeat a parent criterion across sub-units, but each repeated
criterion must describe the local slice.

Example HTTP dispatch:

```bash
PORT="${BBOX_PORT:-7264}"
DOC="design/operations/config-artifacts/ops-artifact-bundles-and-doctor-impl.md"

jq -n \
  --rawfile phase_doc "$DOC" \
  --arg phase_doc_path "$DOC" \
  --arg project_dir "$PWD" \
  '{
    workflow_id: "phase-decompose-main-edit",
    project_dir: $project_dir,
    max_steps: 200,
    await_completion: false,
    initial_vars: {
      phase_doc_path: $phase_doc_path,
      phase_doc_text: $phase_doc,
      project_dir: $project_dir,
      target_context_window: 10000,
      epoch: 0,
      max_epochs: 3,
      acceptance_criteria: [
        {"id":"AB-P0","criterion_text":"Baseline current behavior and source anchors before implementation."},
        {"id":"AB-P1","criterion_text":"Runtime paths, restore, relocation, uninstall, and status are implemented and tested."},
        {"id":"AB-P2","criterion_text":"Artifact kinds, metadata, scoped listing, activators, and version adoption are implemented and tested."},
        {"id":"AB-P3","criterion_text":"Bundle manifest planning, apply, generation records, runtime drift, watcher, and defaults manifests are implemented and tested."},
        {"id":"AB-P4","criterion_text":"Redundant lifecycle tool surfaces are removed or redirected and docs/tool catalog naming is consistent."},
        {"id":"AB-P5","criterion_text":"Doctor read-only status surface replaces scattered manual smoke checks."},
        {"id":"AB-P6","criterion_text":"Upgrade helper provides read-only checks, gated apply mode, operation logging, and recovery."},
        {"id":"AB-P7","criterion_text":"Dependency extraction hardening improves ordering and preflight quality."}
      ]
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

1. `phase-decompose-discovery` extracts question shapes, dispatches
   `corpus-pathfinder` scouts, measures resolved refs with `bbox_ref_size`, and
   emits an evidence bundle plus `fit_direct` or `needs_decompose`.
2. `phase-decompose-ensemble-decompose` opens a whiteboard, gathers blind
   architecture/implementation/risk proposals, resolves challenges, and
   synthesizes a measured DAG. `lint-dag.py` enforces acceptance coverage,
   measured bytes, dependency shape, and degraded-ref handling.
3. `phase-decompose-supervised-impl-edit` runs the bounded implementer for each
   direct or foreach slice, captures git status/diff evidence, and asks the
   no-edit advisor to judge the slice.
4. `phase-decompose-recompose` evaluates the collected sub-results against the
   DAG recompose contract and returns `satisfied`, `work_remains`, or
   `untenable`.
5. `phase-decompose-main-edit` either exits satisfied, halts, or feeds the
   remediation packet back into another epoch until `max_epochs`.

## Operator Rules

- Start from a clean or intentionally understood worktree. The edit lane can
  write files, but it must not own unrelated dirty state.
- Use `phase-decompose-main` first when changing the decomposer artifacts
  themselves. Use `phase-decompose-main-edit` for implementation work.
- Do not dispatch by raw workflow file for real work; use installed workflow ids
  so subworkflow refs resolve against the same catalog the daemon uses in
  production.
- Do not treat `work_remains` as success. Inspect the remediation packet and the
  arc notes, then either rerun with a higher `max_epochs` or fix the blocker.
- Do not archive or close the source implementation doc until the final
  recompose verdict is `satisfied` and the requested validation has run.
- Commit remains operator-owned. The edit lane should leave a diff plus
  validation evidence; committing/pushing is a separate explicit step.

# Performance Pathology Dispatch

Performance pathology is the sibling of
[Architecture Pathology Dispatch](pathology-dispatch.md). It turns static
performance smells plus operator-supplied runtime evidence (profile excerpts,
query logs, benchmark output, metrics) into a reviewed performance correction
plan. It is diagnostic only while it runs: it does not edit source during
diagnosis. Its output is automation-ready for PD-dispatch once the operator
chooses to launch implementation.

The workflow ships one language-neutral lane:

- `perf-pathology` surveys operator-named hot paths or scoped code, dispatches
  the justified subset of performance detector atoms, reviews their claims on a
  whiteboard, and writes a correction plan under `design/refactor/perf/plans/`.

Strongly prefer candidates with runtime corroboration. A static-only smell is a
hypothesis, not a remediation slice, unless the hot-path context is unambiguous.
Do not use pathology as a micro-optimization pass: if a lint or a compiler
already flags the issue, fix it directly. Performance pathology is for
cost-center judgments — n+1 fetches, super-linear loops, eager materialization,
blocking/serial async, and unbounded growth — grounded in evidence.

## Install Artifacts

Install or refresh the performance pathology artifacts before dispatching
`perf-pathology`:

```text
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/performance-pathologist.json")

bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-nested-iteration-complexity.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-eager-materialization.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-n-plus-one-fetch.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-blocking-and-sequential-async.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-unbounded-growth.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/perf-runtime-evidence-corroboration.json")

bbox_artifact_install(kind="workflow", source="system-defaults/workflows/refactor/perf-pathology.json")
```

If a dispatch reports an unknown atom, brofile, or workflow, install the named
artifact and rerun. If it reports an unknown hook operation
(`normalize_perf_pathology_atom_requests` or `write_perf_pathology_plan`),
restart the daemon after installing the current `blackboxd`; pathology uses
native workflow hooks for atom-request normalization and plan writing.

## Invocation

Use `/orchestrate/by-id` or MCP `bro_orchestrate_run` with
`workflow_id = "perf-pathology"`. `bro orchestrate run <file>` is useful for
dry-run validation, but real runs should use the installed workflow id so atom
and brofile references resolve through the production artifact catalog.

Required initial vars:

```json
{
  "project_dir": "/repo",
  "scope_filter": ".",
  "target_context_window": 10000,
  "hot_paths": [
    "src/orders/create.rs"
  ],
  "operator_hints": [
    "order creation request is slow under load"
  ],
  "baseline_refs": [
    "design/refactor/perf/baselines/order-creation-querylog.txt"
  ]
}
```

`scope_filter` may be a package, directory, file, or `"."` for a broad pass.
Use `hot_paths` when you already know the slow request paths or files. Use
`operator_hints` for prior pain, profiler observations, or workload context. Use
`baseline_refs` to point at the runtime evidence the corroboration atom should
read — profiles, query logs, benchmark output, or metric dumps committed under
`design/refactor/perf/baselines/` or referenced from elsewhere. When
`baseline_refs` is empty the run is **advisory**: it produces investigation or
baseline-capture slices before recommending behavior-changing optimization.

Example HTTP dispatch:

```bash
PORT="${BBOX_PORT:-7264}"
PROJECT="/repo"

jq -n \
  --arg project_dir "$PROJECT" \
  '{
    workflow_id: "perf-pathology",
    project_dir: $project_dir,
    max_steps: 80,
    await_completion: false,
    initial_vars: {
      project_dir: $project_dir,
      scope_filter: ".",
      target_context_window: 10000,
      hot_paths: ["src/orders/create.rs"],
      operator_hints: ["order creation request is slow under load"],
      baseline_refs: ["design/refactor/perf/baselines/order-creation-querylog.txt"]
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
2. `Survey` does cheap grounding first: code symbols, refs, usages, and refactor
   status to find loops, fetch sites, async awaits, growing containers, and
   serialization round-trips, plus transcript search for prior performance
   pressure. It reads `baseline_refs` where present and selects only the atom
   requests justified by evidence, always including
   `perf-runtime-evidence-corroboration` when `baseline_refs` is non-empty.
3. `FocusedAtoms` dispatches the selected detector atoms in parallel and collects
   their structured diagnosis results. A broad run may still dispatch fewer than
   all atoms; that is expected.
4. `Review` merges overlapping claims, rejects micro-optimization noise and
   static-only candidates the corroboration atom could not promote (unless the
   hot-path context is unambiguous), distinguishes observed cost from possible
   cost, and advances the whiteboard from blind to read/debate/resolve.
5. `SynthesizePlan` writes strict JSON for a correction plan: diagnosis summary,
   evidence (with a `### Baseline` subsection), ordered remediation slices ranked
   by expected benefit/risk, delta-based acceptance criteria with `PP-*` ids and
   verification methods, and deferred candidates.
6. `WritePlan` writes markdown to
   `<project>/design/refactor/perf/plans/<slug>.md`.

The generated plan is proposed, not executed by the pathology run itself.

## Performance Detector Atoms

The survey may choose from these atoms:

- `perf-nested-iteration-complexity`: avoidable super-linear CPU work — nested
  iteration over the same/related collections, loop-invariant recomputation, or
  cartesian/product expansion without a narrowing predicate.
- `perf-eager-materialization`: an intermediate collection materialized before a
  filter/take/streaming boundary, or a redundant parse/stringify/encode/decode
  round-trip within one scope.
- `perf-n-plus-one-fetch`: repeated per-row DB/RPC fetches where batching or
  eager loading should exist, or query evidence pointing at a missing covering
  index.
- `perf-blocking-and-sequential-async`: blocking IO inside a request,
  event-loop, or async hot path, or independent awaitables awaited serially.
- `perf-unbounded-growth`: a collection, cache, queue, or accumulator that grows
  without an obvious bound or eviction.
- `perf-runtime-evidence-corroboration`: cross-cutting corroborator that reads
  the operator's runtime evidence, promotes static candidates to observed-cost
  or refutes them, and captures the measured baseline.

## Output

Successful runs write a plan with frontmatter like:

```yaml
kind: performance-correction-plan
lifecycle: proposed
corpus: project-refactor
generated_by: perf-pathology
baseline_commit: <sha>
```

The body contains `Diagnosis Summary`, `Evidence` (with a `Baseline`
subsection), optional `Authority Grades` / `Atom Mapping`, `Remediation Plan`,
`Acceptance Criteria` (`PP-*`), `Deferred`, and a `Dispatch Payload`.

## Remediation Handoff

Use the generated plan as the phase document for PD-dispatch. The `Dispatch
Payload` section in the generated plan is the automation handoff: review it,
tighten the delta-based acceptance criteria and their verification methods, then
run it through PD-dispatch. Performance acceptance criteria often require a
benchmark, query-log replay, or profile recapture to verify; state per-slice
whether verification is automated or manual. See
[Phase-Decomposer Dispatch](pd-dispatch.md) for the implementation lane.

## Operator Rules

- Start from an indexed project. If the survey reports weak code index coverage,
  reindex/reembed before treating a broad negative result as meaningful.
- Supply `baseline_refs` whenever runtime evidence exists. Without it the plan is
  advisory and should produce baseline-capture slices first.
- Prefer `hot_paths` and `scope_filter` when you already know the slow path.
  Whole-project performance sweeps are noisy and weakly grounded.
- Treat "fewer atoms dispatched" as normal. The survey should not run all atoms
  just because they exist.
- Do not accept static-only smells as observed cost. Distinguish observed cost
  from possible cost in every retained claim.
- Do not edit code in pathology. The output is a correction plan; implementation
  belongs to PD-dispatch or an explicitly scoped manual edit.
- Commit remains operator-owned. Pathology may create a plan file in the target
  project, but committing it is a separate explicit step.

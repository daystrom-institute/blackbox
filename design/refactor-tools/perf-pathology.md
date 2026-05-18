---
title: "Performance Pathology"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
  - performance
  - pathology
date: 2026-05-16
status: "proposal, awaiting review"
brief: "Sibling to Architecture Pathology: scout performance smells, validate and cluster the evidence, then emit a reviewed performance correction plan for phase-decompose remediation."
---

# Performance Pathology

Performance pathology is the performance-focused sibling of
[Architecture Pathology](arch-pathology.md). It uses the same high-level shape:
diagnose first, emit a correction plan, then let a reviewed PD-style
remediation dispatch implement the plan.

It does not introduce a custom PD input shape. The durable handoff is a
performance correction plan document plus explicit acceptance criteria.

## Motivation

Performance debt usually lives in tickets, benchmark notes, slow-query logs, or
chat threads. Those records are hard to turn into bounded remediation work.
Performance pathology turns profiling and static smell evidence into a plan:
what is slow or wasteful, why the evidence is credible, what slice should be
fixed first, and how to measure success.

## Scope and non-goals

In scope:

- Scouting and detector runs over operator-named hot paths or scoped code.
- Candidate detection for a small v0 smell set: nested iteration,
  loop-invariant recomputation, cartesian explosion, redundant serialization,
  unbounded growth, n+1 calls, sequential awaitables, sync IO in hot paths,
  missing covering indexes, and eager materialization.
- Combining static evidence with operator-provided runtime evidence such as
  profile excerpts, query logs, benchmark output, or metrics.
- Emitting a performance correction plan with delta-based acceptance criteria.

Out of scope for v0:

- Automated profile or query-log ingestion for arbitrary tools.
- Automated benchmark execution as part of diagnosis.
- A new canonical diagnosis store or permanent candidate ID lifecycle.
- CI performance gating.
- Workload-aware ranking beyond the evidence the operator supplies.

## Workflow shape

```
perf-pathology workflow
  input phase ......... project_dir, scope_filter, hot_path hints,
                         optional baseline/profile/log references
  discovery phase ..... corpus-pathfinder scouts ground code regions and
                         likely hot paths
  detector phase ...... static and evidence-aware detectors post candidate
                         performance smells
  ensemble phase ...... validate evidence, reject static-only noise, cluster
                         remediation slices, rank by expected benefit/risk
  emit phase .......... write a performance correction plan with explicit
                         acceptance criteria for phase-decompose-main-edit

[operator review]

phase-decompose-main-edit or a perf-aware wrapper
  remediates the reviewed plan using explicit acceptance_criteria
```

The workflow can run in advisory mode when no runtime baseline exists. Advisory
mode should produce investigation or measurement slices before recommending
behavior-changing optimization work.

## Plan document shape

Path:

```text
<project>/design/refactor/perf/plans/<slug>.md
```

Required frontmatter:

```yaml
---
title: "Performance Correction Plan: <scope>"
kind: performance-correction-plan
lifecycle: proposed
corpus: <project>-refactor
topic:
  - refactor-plan
  - performance
date: <YYYY-MM-DD>
baseline_commit: <full-sha>
generated_by: perf-pathology
scope: "<operator-readable scope>"
brief: "<one-line>"
---
```

These frontmatter fields are operator/audit metadata for the correction plan.
They are not consumed by current PD tooling unless a future wrapper explicitly
chooses to read them.

Required body sections:

- `## Diagnosis Summary` - short summary of the cost centers and remediation
  direction.
- `## Evidence` - static code references plus any profile, query-log,
  benchmark, or metric evidence supplied by the operator.
- `## Baseline` - current measurements when available, or an explicit statement
  that the plan is advisory and baseline capture is the first slice.
- `## Remediation Plan` - ordered slices suitable for PD.
- `## Acceptance Criteria` - stable criteria with IDs such as `PP-1`, `PP-2`.
- `## Deferred` - candidates rejected or postponed.

Optional convenience section:

- `## Dispatch Payload` - a copy-pasteable example of normal PD initial vars,
  including explicit `acceptance_criteria`. This is not a parsing target or
  durable artifact.

Example acceptance criteria:

```json
[
  {
    "id": "PP-1",
    "criterion_text": "Order creation no longer performs per-row product fetches in the identified request path."
  },
  {
    "id": "PP-2",
    "criterion_text": "Query count for the documented order creation fixture drops from 47 to no more than 3."
  },
  {
    "id": "PP-3",
    "criterion_text": "Existing order creation behavior tests still pass after the eager-load or batching change."
  }
]
```

## Evidence model

Evidence is plan prose. A candidate should identify:

- code references
- runtime evidence source, when available
- baseline value and measurement method, when available
- expected improvement
- confidence and uncertainty
- verification cost
- proposed remediation direction

The ensemble should strongly prefer candidates with runtime corroboration. A
static-only performance smell is usually a hypothesis, not a remediation slice,
unless the hot-path context is unambiguous.

## Smell catalog

The v0 catalog is intentionally small. It names review targets, not a permanent
ontology.

| Smell kind | Notes |
|---|---|
| `nested_iteration_over_same_collection` | Likely avoidable O(n^2) work. |
| `loop_invariant_recomputation` | Repeated pure work inside a loop. |
| `cartesian_explosion` | Pair/product expansion without a narrowing predicate. |
| `redundant_serialization` | Parse/stringify or encode/decode roundtrip in one scope. |
| `unbounded_growth` | Collection, cache, queue, or accumulator without an obvious bound. |
| `n_plus_one` | Repeated DB/RPC fetches where batching or eager loading should exist. |
| `sequential_await_batchable` | Independent async operations awaited serially. |
| `sync_io_in_hot_path` | Blocking IO inside request, event-loop, or async hot path. |
| `missing_covering_index` | Query evidence suggests an index or database design issue. |
| `eager_materialization` | Intermediate collection materialized before filter/take/streaming boundary. |

## Remediation handoff

The remediation handoff is the same as architecture pathology: a reviewed plan
document plus explicit acceptance criteria passed to `phase-decompose-main-edit`.

Performance plans may need a perf-aware verification wrapper later, because
delta checks often require benchmarks, query-log replay, or profile capture.
That wrapper should sit around PD execution; it should not change the diagnosis
artifact into a separate machine-readable model.

## Per-project adoption

1. Create `<project>/design/refactor/perf/`.
2. Capture whatever baseline evidence already exists under
   `<project>/design/refactor/perf/baselines/` or reference external evidence
   from the plan.
3. Dispatch `perf-pathology` with `project_dir`, `scope_filter`, and hot-path
   hints.
4. Review the generated correction plan. Delete speculative slices, tighten
   metrics, and confirm measurement methods.
5. Dispatch remediation through `phase-decompose-main-edit` or a perf-aware
   wrapper that supplies the same PD initial vars plus measurement hooks.
6. Re-measure and rerun pathology when the baseline moves.

## Risks and design choices

**Static-only false positives.** Performance smells need workload context. The
plan must distinguish "observed cost" from "possible cost."

**Measurement cost.** Delta acceptance criteria can be expensive to verify.
Plans should state whether verification is per-slice, batched, or manual.

**Baseline drift.** Baselines are tied to commit and workload. A plan without a
fresh baseline should be treated as advisory.

**Premature abstraction.** Architecture and performance pathology share a
workflow shape, but the evidence and verification concerns differ. Keep them as
sibling designs until real artifact usage shows what deserves a shared
framework.

## Rejected alternatives

A performance-specific machine-readable artifact model was rejected for the same
reason as the architecture equivalent: it invents a second PD handoff shape
before the diagnosis loop has proven useful.

Automated profile/query-log ingestion was rejected for v0. Tooling sprawl is
large; operator-supplied evidence is enough to validate the workflow shape.

CI gating was rejected for v0. Report-only diagnostics may come later.

## Future work

- Per-tool evidence importers for common profilers and query-log systems.
- A perf-aware wrapper around PD that runs declared measurement hooks.
- Workload-aware detector ranking.
- Additional language and framework detector packs.
- Report-only regression mode for PRs once precision is proven.

## Relationship to existing designs

- Sibling to [Architecture Pathology](arch-pathology.md).
- Remediation uses [Phase-Decomposer](../orchestration/phase-decomposer/phase-decomposer.md)
  or a small wrapper around the same PD dispatch shape.
- Refactor execution can use [Refactor Agents](refactor-agents.md) and
  [Refactor Compound Runs](refactor-compound-runs.md) when a slice maps to
  existing atom-backed changes.

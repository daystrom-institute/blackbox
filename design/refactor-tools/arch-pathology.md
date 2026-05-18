---
title: "Architecture Pathology"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
  - architecture
  - pathology
date: 2026-05-16
status: "proposal, awaiting review"
brief: "PD-shaped diagnosis workflow: scout a codebase for architecture smells, validate and cluster the evidence, then emit a reviewed correction plan that can be remediated through phase-decompose-main-edit."
---

# Architecture Pathology

Architecture pathology is the upstream half of a refactor remediation loop. It
does not implement changes. It scouts a codebase for architecture smells and
emits a correction plan. The correction plan is then reviewed and fed through
the existing phase-decompose edit lane, or a small wrapper around that lane, for
remediation.

The shape intentionally mirrors [Phase-Decomposer Dispatch](../../docs/pd-dispatch.md):
scouts gather grounded evidence, an ensemble decides whether and how to split
the work, and the workflow emits a bounded document plus acceptance criteria.
The difference is the output: PD emits/executes implementation slices, while
pathology emits the remediation plan that PD later executes.

## Motivation

Hand-auditing a codebase for cross-layer coupling, god units, dead exports, and
similar structural smells is slow and leaves little durable evidence. Existing
refactor tooling covers the execution side once a plan exists:
[AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md),
[Refactor Compound Runs](refactor-compound-runs.md), and
[Refactor Agents](refactor-agents.md). The missing step is diagnosis:
turn "this codebase has architectural problems" into "here is a bounded
correction plan with concrete acceptance criteria."

## Scope and non-goals

In scope:

- Running a PD-shaped diagnosis workflow over a target project, package, module,
  or operator-named hotspot.
- Using scouts and detector agents to collect evidence for a closed v0 smell
  set: layer violation, dead export, god unit, runtime type discrimination,
  global state escape, and scope mismatch.
- Having an ensemble validate, cluster, rank, and bound the candidate smells
  into remediation slices.
- Emitting one correction plan document with the explicit acceptance criteria
  PD needs for remediation.

Out of scope for v0:

- Auto-execution. Pathology stops at plan emission. Remediation is a separate
  reviewed dispatch.
- A new canonical diagnosis store.
- Teaching `phase-decompose-main-edit` to parse pathology-specific artifacts.
  Existing PD inputs remain explicit.
- New refactor transform atoms. The plan can recommend existing atom-backed
  remediations or mark work as manual.
- CI gating. The first useful version is an operator-run diagnostic, not a
  build breaker.

## Workflow shape

```
arch-pathology workflow
  input phase ......... project_dir, scope_filter, optional layer model,
                         optional operator hints
  discovery phase ..... corpus-pathfinder scouts ground code regions,
                         symbols, dependency paths, and prior context
  detector phase ...... parallel smell detectors inspect the grounded scope
                         and post candidate evidence to a whiteboard
  ensemble phase ...... reviewers validate candidates, merge duplicates,
                         reject weak evidence, cluster remediation slices,
                         rank by impact and blast radius
  emit phase .......... write correction-plan.md with explicit acceptance
                         criteria for phase-decompose-main-edit

[operator review]

phase-decompose-main-edit
  receives the correction plan as phase_doc_text and the generated acceptance
  criteria as normal initial_vars.acceptance_criteria
```

The boundary is deliberately plain. The artifact crossing from pathology to
remediation is a correction plan plus acceptance criteria, not a new diagnostic
store.

## Inputs

The workflow accepts the same kind of explicit initial variables as PD:

```json
{
  "project_dir": "/repo",
  "scope_filter": "src/main/java/com/example/admin",
  "target_context_window": 10000,
  "layer_model_path": "design/refactor/layer-model.md",
  "operator_hints": [
    "admin UI has backend session coupling",
    "prefer service extraction over broad package reshuffle"
  ]
}
```

`layer_model_path` is optional. The example path is a project-side convention,
not a repo-global file that must already exist. When present, it helps
layer-violation detectors. When absent, the workflow can still scout generic
smells such as god units, runtime type discrimination, and suspicious global
state access.

## Plan document shape

Pathology writes a correction plan document:

```text
<project>/design/refactor/plans/<slug>.md
```

The document is a normal phase document. It should be readable on its own and
usable as `phase_doc_text` for `phase-decompose-main-edit`.

Required frontmatter:

```yaml
---
title: "Architecture Correction Plan: <scope>"
kind: correction-plan
lifecycle: proposed
corpus: <project>-refactor
topic:
  - refactor-plan
  - architecture
date: <YYYY-MM-DD>
baseline_commit: <full-sha>
generated_by: arch-pathology
scope: "<operator-readable scope>"
brief: "<one-line>"
---
```

These frontmatter fields are operator/audit metadata for the correction plan.
They are not consumed by current PD tooling unless a future wrapper explicitly
chooses to read them.

Required body sections:

- `## Diagnosis Summary` - short operator-readable summary of the smell clusters
  and the intended remediation direction.
- `## Evidence` - concrete file, symbol, import, usage, or call-path evidence.
  Evidence is prose and code references, not machine-owned data.
- `## Remediation Plan` - ordered slices that PD can implement.
- `## Acceptance Criteria` - stable criteria with IDs such as `AP-1`, `AP-2`.
- `## Deferred` - candidates rejected or postponed by the ensemble.

Optional convenience section:

- `## Dispatch Payload` - a copy-pasteable example of normal PD initial vars.
  This is not a parsing target or durable artifact. The canonical
  remediation input is still the correction plan text plus explicit acceptance
  criteria.

Example acceptance criteria:

```json
[
  {
    "id": "AP-1",
    "criterion_text": "Backend packages no longer import UI-layer types identified in the diagnosis evidence."
  },
  {
    "id": "AP-2",
    "criterion_text": "The extracted adapter/service compiles and existing login/admin flow tests still pass."
  },
  {
    "id": "AP-3",
    "criterion_text": "Any public API or visibility changes are explicitly surfaced for operator approval before execution."
  }
]
```

## Smell taxonomy

The v0 catalog is intentionally small. It names review targets, not a permanent
ontology.

| Smell kind | Notes |
|---|---|
| `layer_violation` | Dependency edge crosses an operator-declared or inferred architectural boundary. |
| `dead_export` | Visibility exceeds observed usage and callers do not justify the public surface. |
| `god_unit` | Class/module/package has anomalous responsibility, size, or fan-in/fan-out relative to local norms. |
| `runtime_type_discrimination` | Dispatch by runtime type/identity where polymorphism or a strategy boundary would fit better. |
| `global_state_escape` | Service locator or global mutable state access outside bootstrap or approved adapter code. |
| `scope_mismatch` | Narrower-lifetime resource captured by wider-lifetime consumer in DI or supervision-like systems. |

Language-specific detectors are implementation details of the detector agents.
The correction plan should explain the smell in project terms rather than expose
detector internals.

## Detector guidance

Detector agents produce candidate evidence for the ensemble, not durable
records. A detector result should contain:

- `smell_kind`
- concise summary
- concrete code references
- evidence snippets or dependency paths
- confidence and uncertainty
- suggested remediation direction
- risks, especially public API or lifecycle changes

The ensemble is responsible for rejecting weak candidates, merging duplicates,
and deciding which candidates become remediation slices.

## Remediation handoff

Remediation uses the existing PD invocation shape documented in
[pd-dispatch.md](../../docs/pd-dispatch.md). Pathology does not require PD to
learn pathology-specific fields.

Minimum handoff:

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
      { "id": "AP-1", "criterion_text": "..." }
    ]
  }
}
```

A future wrapper may read the correction plan and build this payload
mechanically. That wrapper is convenience, not a new artifact model.

## Per-project adoption

1. Create or reuse `<project>/design/refactor/`.
2. Optionally write `<project>/design/refactor/layer-model.md` with the
   project boundaries the operator cares about.
3. Dispatch `arch-pathology` with `project_dir`, `scope_filter`, and any
   operator hints.
4. Review the generated correction plan. Delete weak slices, adjust order, and
   tighten acceptance criteria.
5. Dispatch `phase-decompose-main-edit` with the reviewed plan text and the
   reviewed acceptance criteria.
6. If PD returns `work_remains`, either rerun PD with a higher epoch ceiling or
   rerun pathology against the new baseline for a fresh correction plan.

## Risks and design choices

**False positives.** Architecture smells are judgment calls. The ensemble must
prefer fewer, better-supported slices over an exhaustive smell dump. Weak or
speculative candidates belong in `Deferred`.

**Layer model quality.** Layer-violation detection is only as good as the model
or hints it receives. When the model is absent or vague, the plan should say so
and avoid pretending inferred boundaries are authoritative.

**Baseline drift.** The correction plan names a baseline commit. Before
remediation, the operator or wrapper should check whether touched loci have
moved substantially. If they have, rerun pathology or refresh the plan.

**Public API and operator authority.** Pathology may recommend visibility,
interface, or extraction work. The correction plan must surface these as
operator decisions. It must not imply atoms may set authority flags such as
`acknowledge_public_api_change` on the operator's behalf.

**Atom coverage.** Some remediations map cleanly to existing refactor atoms;
others are manual. The plan should name likely atoms when known, but PD
execution remains responsible for grounding the actual code edits.

## Rejected alternatives

A pathology-specific machine-readable artifact model was rejected. It creates a
second handoff shape beside PD-dispatch before the basic diagnostic loop has
proven useful.

A dedicated graph entity type for individual smell candidates was rejected for
v0. If org-wide rollups become important later, they can index correction plan
documents after the workflow has real usage.

Auto-dispatch from pathology directly into remediation was rejected. Diagnosis
quality depends on operator taste; the correction plan is a review point, not a
rubber stamp.

## Future work

- Detector packs for additional languages and frameworks.
- A helper that proposes a layer model from package/module conventions.
- A wrapper that reads a reviewed correction plan and launches
  `phase-decompose-main-edit` with generated initial vars.
- Advisory CI mode that reports newly introduced smell candidates without
  blocking builds.
- Cross-repo reporting over checked-in correction plans, if enough plans exist
  to justify an index.

## Relationship to existing designs

- Sibling-shaped to [Phase-Decomposer](../orchestration/phase-decomposer/phase-decomposer.md):
  same broad dispatch discipline, different output.
- Upstream of [AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md)
  and [Refactor Agents](refactor-agents.md): pathology proposes; those surfaces
  execute or assist execution.
- Related to [Refactor Compound Runs](refactor-compound-runs.md): PD may choose
  compound refactor runs for slices that map to existing atoms.
- Per-project artifacts live under `<project>/design/refactor/` and follow the
  [Design Corpus](../design-corpus.md) frontmatter conventions.

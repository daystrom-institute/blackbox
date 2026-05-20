---
title: "Architecture Pathology"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
  - architecture
  - pathology
date: 2026-05-16
status: "implemented; archived after code audit"
brief: "PD-shaped forensic architecture workflow: spend LLM time only on bad-code and bad-architecture diagnoses that SAST cannot make, then emit a reviewed correction plan for phase-decompose remediation."
---

# Architecture Pathology

Architecture pathology is the upstream diagnostic lane for refactoring. It
figures out what is architecturally wrong, argues the evidence, and emits a
reviewable correction plan. It does not edit code.

This is not a SAST replacement. If a conventional static analyzer, ArchUnit
rule, lint, clone detector, or metric pass can make the diagnosis, pathology
should not spend LLM time on it. Pathology is for the cases static tools can
only point near: role mismatch, semantic duplication, framework lifecycle
misuse, test-implied seams, and prior operator pain that needs to be connected
back to current code.

After operator review, the correction plan is handed to
[Phase-Decomposer Dispatch](../../docs/pd-dispatch.md) as normal
`phase_doc_text` plus explicit `acceptance_criteria`.

## Motivation

Existing refactor tooling covers execution once a plan exists:
[AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md),
[Refactor Compound Runs](refactor-compound-runs.md), and
[Refactor Agents](refactor-agents.md). PD can decompose and implement a large
plan through supervised edit lanes. The missing part is a disciplined way to
produce the plan when the problem is not "rename this" or "extract that", but
"this area has rotted and I need to know what the real correction is."

The useful version is not a detector catalog. The useful version is a forensic
loop: measure cheap facts, ask LLM-worthy diagnostic questions, challenge the
answers, request more measurements when the first explanation is weak, and
render only the diagnoses that survive into a correction plan.

## Scope and non-goals

In scope:

- Java v0, especially framework-heavy Java stacks such as Spring, Guice,
  Vaadin, JOOQ, servlet/session code, and test-heavy service layers.
- Whole-project damage assessment when the operator asks for broad triage,
  with bounded hotspot selection instead of all-atom/all-file fan-out.
- Code navigation and measurement over symbols, imports, calls, fields, tests,
  refactor analysis plans, git history, and Blackbox transcript history.
- A small set of pathology atoms whose diagnoses require semantic judgment
  beyond SAST.
- Whiteboard-backed review where specialists challenge, corroborate, merge, or
  reject candidate diagnoses before a plan is written.
- One correction plan document with concrete evidence, ordered remediation
  slices, and PD-ready acceptance criteria.

Out of scope for v0:

- Auto-execution. Pathology stops at plan emission.
- Reimplementing SAST, ArchUnit, clone detection, or metric dashboards.
- A durable `Finding` entity, finding sidecar, or new schema that PD must parse.
- CI gating.
- Local hygiene refactors that do not change architectural ownership, seams, or
  contracts.

## SAST gate

Every candidate atom must pass this gate:

1. **Static tools cannot answer the diagnostic question.** They may provide raw
   measurements, but they cannot decide the architectural fact.
2. **The LLM judgment changes the diagnosis, not just the wording.** "This file
   has high fan-out" is a metric. "This service owns presentation policy because
   its methods format locale-specific UI strings despite legal imports" is a
   pathology claim.
3. **The remediation is architectural.** It changes ownership, boundaries,
   seams, lifecycle handling, or canonical responsibility. Local cleanup belongs
   elsewhere.

Candidates that fail this gate are not v0 pathology atoms, even if they are
useful refactor hints.

## Workflow shape

Pathology is PD-shaped in orchestration discipline, not in output. PD turns an
implementation document into work slices; pathology turns architectural
uncertainty into the implementation document PD should later consume.

```text
arch-pathology
  input ............... project_dir, scope_filter, optional operator hints,
                         optional layer/model notes, target context window
  cheap survey ........ symbols, imports, refs, calls, fields, tests, history,
                         transcripts, existing refactor analysis reports
  hypothesis loop ..... a pathologist selects one or more atoms, asks for
                         targeted measurements, posts a diagnosis candidate,
                         and requests another measurement when evidence is weak
  whiteboard review ... specialists challenge/corroborate candidates, merge
                         overlapping claims, reject SAST-shaped findings, and
                         choose the correction-plan slices
                         weak or conflicting evidence loops back to targeted
                         measurement instead of becoming a finding dump
  emit ................ correction plan markdown with evidence, remediation
                         slices, and PD-ready acceptance criteria

[operator review]

phase-decompose-main-edit
  receives the reviewed plan as phase_doc_text and acceptance criteria as
  normal initial_vars.acceptance_criteria
```

This is deliberately iterative. The workflow does not run eight atoms in a
static fan-out and ask an ensemble to vote on the dump. It starts with the
operator's suspicion or the cheap survey, runs the atoms that can answer the
next diagnostic question, and loops until the plan has enough corroborated
evidence to be worth editing code.

Whiteboard posts are deliberation artifacts, not durable finding records. The
post body should be prose: claim, code references, measurements, uncertainty,
and proposed correction. Posts may link related claims for review and conflict
detection, but the correction plan remains the durable handoff.

## Whole-project damage assessment

When the operator says "the scope is the whole project, give me the damages",
pathology must switch from local diagnosis to bounded triage. It still must not
run every atom over every file or emit a smell inventory.

Whole-project mode has a stricter shape:

1. **Cheap survey.** Build a project-wide pressure map from symbols, imports,
   refs, calls, fields, tests, git history, transcript history, and existing
   analysis-only refactor reports. This pass may be broad because it is mostly
   mechanical.
2. **Hotspot selection.** Rank candidate loci by corroborated pressure:
   transcript/operator pain, fix/revert loops, growth and co-change, role
   outliers, heavy test setup pain, likely framework lifecycle risk, and dense
   responsibility overlap. Pick a bounded top set for LLM diagnosis.
3. **Focused atom runs.** Run only the atoms that can answer the next question
   for each hotspot. A lifecycle-looking hotspot does not need conceptual
   duplicate discovery; a transcript-anchored pain point may need role-behavior
   and test-implied architecture first.
4. **Cluster synthesis.** Merge related atom outputs into damage clusters.
   Multiple atom signals on one locus become one diagnosis. Multiple loci with
   the same responsibility owner problem become one diagnosis.
5. **Damage report.** Emit a correction plan that lists the top damage clusters,
   evidence, uncertainty, expected blast radius, recommended remediation order,
   and deferred candidates. The plan should normally cap itself to the few
   clusters worth acting on first.

The output is not "438 smells". It is closer to:

```text
The worst architectural damage is concentrated in:

1. Session/UI context ownership collapse around SessionData and admin services.
2. Account validation policy duplicated across service, UI, and import paths.
3. Service tests exposing a missing request/session adapter boundary.

Here is the evidence, what to fix first, what to defer, and the acceptance
criteria for the first PD remediation slices.
```

Whole-project mode may produce a short appendix of deferred hotspots, but
deferred means "not enough corroborated evidence or not first-order damage",
not "all low-severity findings".

## Inputs

The workflow accepts explicit initial variables like PD:

```json
{
  "project_dir": "/repo",
  "scope_filter": "src/main/java/com/example/admin",
  "target_context_window": 10000,
  "layer_model_path": "design/refactor/layer-model.md",
  "operator_hints": [
    "admin UI has backend session coupling",
    "SessionData might be the worst offender"
  ]
}
```

`layer_model_path` is optional. When present, it helps with context and
correction-plan wording; v0 atoms must still pass the SAST gate. Declared layer
violations by themselves are not pathology findings.

## Shared measurement substrate

The atoms should share cheap measurements before spending LLM context on deep
reads:

- `bbox_code_symbols` for project-wide symbol search and line ranges.
- `bbox_code_refs` per file for imports, calls, fields, and identifiers.
- `java_class_dependency_analysis` for analysis-only class dependency reports
  when a class is a candidate locus.
- LSP references, definitions, call hierarchy, and hover data where available.
- Git history for growth, co-change, fix/revert patterns, and commit narrative.
- Test-to-production mapping by naming convention and references.
- Transcript and knowledge retrieval for prior operator complaints, decisions,
  notes, and abandoned work threads.

The shared substrate should produce candidate loci and measurements. The atoms
decide whether those measurements mean anything architecturally.

## V0 pathology atoms

### 1. Role-Behavior Coherence

Diagnostic question: does this class do what its name, package, annotations,
interfaces, and docs claim it does?

SAST can count imports and dependencies. It cannot decide that an `@Service`
has become a UI renderer, cookie manager, persistence adapter, and session
orchestrator despite using only legal imports. This atom classifies method
behavior into responsibility families, compares those families to the declared
role, and proposes role-shaped extractions.

Correction-plan output: intended role, actual behavior clusters, foreign
clusters, and extraction slices that leave the original class with the role it
claims.

### 2. Responsibility Bleed

Diagnostic question: is one conceptual responsibility scattered across multiple
units with no canonical home?

This is cross-unit ownership drift. The same responsibility may appear as tax
rules in a service, formatter, export path, and report builder, each with small
semantic differences. Static tools can show call graphs and dependency overlap;
they cannot name the conceptual responsibility or decide which unit should own
it.

Correction-plan output: the scattered responsibility, all loci, divergences
that must be preserved or fixed, the proposed owner, and migration order.

### 3. Conceptual Duplicate Discovery

Diagnostic question: do unrelated classes or methods solve the same
architectural problem under different names, signatures, or call paths?

Clone detectors find lexical or AST similarity. They do not find paraphrase
duplicates where `validateUser`, `checkAccount`, and `authorizeCustomer` enforce
the same policy through different helpers and exception shapes. This atom
clusters behavior by semantic purpose, dependency families, tests, and domain
language.

Correction-plan output: duplicate cluster, canonical behavior, differences that
are bugs vs intentional variants, and redirect/delete slices.

### 4. Anemic Data / Remote Behavior

Diagnostic question: does behavior live on the wrong side of a data-behavior
split?

SAST can flag data-only classes and count getter calls. It cannot decide that
`OrderProcessor` is effectively the missing behavior of `OrderData`, or that
three helpers are mining the same DTO because the domain object has no methods.
This atom diagnoses the pair or cluster relationship, not the individual class
metric.

Correction-plan output: data holder, remote behavior owners, access density,
legitimate framework-data cases to leave alone, and slices that move or
consolidate behavior around the domain concept.

Missing domain-type crystallization is deferred from v0 unless it appears as
part of one of the above diagnoses. A local primitive-to-value-object cleanup is
useful, but it is not enough by itself to justify pathology.

### 5. Scoped-Context Capture

Diagnostic question: is a short-lived runtime context stored into longer-lived
state through a path the framework or DI container cannot police?

Spring, Guice, and similar frameworks can catch some annotated scope mismatches.
The pathology is the invisible path: `UI.getCurrent()`, `VaadinSession`, request
attributes, transactions, `DSLContext`, or security context captured into a
field, cache, async closure, singleton, or static holder. The atom traces
whether the value is stored or transient and whether the storage lifetime is
wider than the captured value.

Correction-plan output: capture site, storage path, holder lifetime, captured
lifetime, predicted failure mode, and a remediation such as parameter passing,
`Provider<T>`, scope bridge, or state relocation.

### 6. Framework Contract Violation

Diagnostic question: is framework API use valid for the caller's role and
lifecycle?

This atom covers implicit framework promises that the framework itself cannot
fully enforce: Spring `@Transactional` self-invocation, Vaadin UI access from
the wrong thread, service-locator use outside bootstrap, JOOQ context lifetime,
Bean Validation group ordering, async closure semantics, and similar contracts.
Static tools can find API calls; they cannot infer whether the caller is a
legitimate bootstrap/adapter/configuration site or a domain/service misuse.

Correction-plan output: framework contract, caller role, why the call violates
or satisfies the contract, and the idiomatic replacement.

### 7. Test-Implied Architecture

Diagnostic question: what architecture do the tests wish production had?

The signal is in test pain: reflection access, `@VisibleForTesting`, widened
visibility, test-only DI overrides, long mock setups, and helpers that exist
only to make a production class testable. Finding those patterns is mechanical;
inferring the missing seam from the test's name, setup, and assertions is not.

Correction-plan output: test intent, workaround technique, production design
gap, and the seam or extraction that would make the test direct.

### 8. Transcript-Anchored Architectural Pressure

Diagnostic question: where has operator or agent history already identified
architectural pain, and does current code confirm it?

Blackbox has transcripts, notes, work threads, decisions, and git provenance.
SAST has none of that. This atom reads the narrative, not just churn counts:
repeated complaints about the same class, failed fix attempts, debates about
ownership, abandoned refactor plans, and postmortems. It then checks whether
the code still matches the complaint and whether the pressure has grown.

Correction-plan output: transcript anchors, dates, current code state, trend,
and how the history corroborates or reorders other atom diagnoses.

## Triangulation example

A useful pathology result is usually a triangulation, not a single atom firing.

Example:

- Role-Behavior Coherence says `SessionData` presents itself as session state
  but also adapts persistence records and reaches into UI/session APIs.
- Scoped-Context Capture says a session or UI object is stored into a
  longer-lived holder rather than acquired at the request boundary.
- Transcript-Anchored Pressure finds prior operator complaints that the same
  class breaks unrelated changes and has grown since the complaint.

The correction plan should not emit three findings. It should emit one
diagnosis: `SessionData` is carrying session state, persistence adaptation, and
UI context ownership in one place. The plan then proposes ordered slices such
as "extract persistence-record adaptation", "move UI/session access behind a
request/session adapter", and "leave the remaining state object with only
session-state behavior", with acceptance criteria for each slice.

## Plan document shape

Path:

```text
<project>/design/refactor/plans/<slug>.md
```

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

- `## Diagnosis Summary` - the surviving architectural diagnoses and why they
  matter.
- `## Evidence` - prose evidence with concrete code references, measurements,
  transcript anchors, and uncertainty.
- `## Remediation Plan` - ordered, bounded slices PD can implement.
- `## Acceptance Criteria` - stable criteria with IDs such as `AP-1`, `AP-2`.
- `## Deferred` - rejected, speculative, SAST-shaped, or v2 candidates.

Optional convenience section:

- `## Dispatch Payload` - a copy-pasteable example of normal PD initial vars.
  This is not a parsing target. The remediation input is the reviewed plan text
  plus explicit acceptance criteria.

Example acceptance criteria:

```json
[
  {
    "id": "AP-1",
    "criterion_text": "The session-state object no longer adapts persistence records or imports the UI/session APIs named in the evidence."
  },
  {
    "id": "AP-2",
    "criterion_text": "UI/session context is acquired at the request/session boundary and passed explicitly or through a scoped adapter."
  },
  {
    "id": "AP-3",
    "criterion_text": "Existing login/admin/session flow tests still pass, and any public API or visibility changes are surfaced for operator approval."
  }
]
```

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

A future wrapper may read a reviewed correction plan and assemble this payload.
That wrapper is convenience around PD, not a new diagnosis artifact model.

## Per-project adoption

1. Create or reuse `<project>/design/refactor/`.
2. Optionally write `<project>/design/refactor/layer-model.md` with declared
   boundaries and framework conventions the operator cares about.
3. Dispatch `arch-pathology` with `project_dir`, `scope_filter`, and any
   operator hints.
4. Review the generated correction plan. Delete weak slices, change ordering,
   and tighten acceptance criteria.
5. Dispatch `phase-decompose-main-edit` with the reviewed plan text and
   acceptance criteria.
6. If PD returns `work_remains`, either rerun PD with a higher epoch ceiling or
   rerun pathology against the new baseline for a fresh correction plan.

## Rejected or deferred from v0

- **Declared layer violations.** Use ArchUnit or an equivalent static rule when
  the layer model is declared. Pathology may reason about semantic bleed despite
  legal imports, but it should not rediscover forbidden imports.
- **Runtime type-switch detection.** SAST can flag long `instanceof` or
  `switch(Class<?>)` chains. LLM time is better spent on remediation design if a
  static tool already found the issue.
- **Public surface and fan-out metrics.** Useful inputs, not pathology atoms.
  Metrics can identify blast radius; they do not decide architectural ownership.
- **Missing domain-type crystallization by itself.** Deferred to v2. It becomes
  v0-relevant when paired with responsibility bleed, conceptual duplication, or
  anemic-data diagnosis.
- **Ceremonial seams.** Deferred to v2. Framework-heavy Java has too many
  legitimate single-implementation interfaces, proxies, generated mappers, and
  adapters for this to earn v0 LLM budget.
- **Catch-and-forget exception sites.** Usually bug-finding or framework
  contract analysis, not standalone architecture pathology.

## Risks and design choices

**SAST collapse.** The largest risk is turning pathology into a bigger lint
run. The gate above is mandatory: if SAST can make the diagnosis, pathology
does not own it.

**False certainty.** Architecture diagnoses are taste-laden. Whiteboard review
must prefer fewer, better-corroborated plan slices over broad smell inventories.

**Measurement cost.** Some atoms need call graphs, LSP references, test reads,
and transcript retrieval. The cheap survey and shared substrate exist to avoid
deep LLM reads until a candidate is worth investigating.

**Operator review.** The correction plan is a review point. Pathology must not
auto-dispatch into remediation.

**Public API and operator authority.** Pathology may recommend visibility,
interface, or extraction work. The plan must surface those decisions. It must
not imply atoms may set authority flags such as
`acknowledge_public_api_change` on the operator's behalf.

## Future work

- Implement Java v0 atom brofiles and shared measurement passes.
- Add language packs for C#, Elixir, Rust, and TypeScript only after the Java
  atom shape proves useful.
- Add a small wrapper that reads a reviewed correction plan and launches
  `phase-decompose-main-edit` with generated initial vars.
- Revisit deferred atoms after real pathology runs show whether their
  LLM-to-signal ratio is acceptable.
- Add advisory reporting over newly introduced architectural pressure after
  operator-run precision is proven.

## Relationship to existing designs

- Sibling-shaped to [Phase-Decomposer](../orchestration/phase-decomposer/phase-decomposer.md):
  same broad dispatch discipline, different output.
- Upstream of [AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md)
  and [Refactor Agents](refactor-agents.md): pathology proposes; those surfaces
  execute or assist execution.
- Related to [Refactor Compound Runs](refactor-compound-runs.md): PD may choose
  compound refactor runs for slices that map to existing atoms.
- Related to [Performance Pathology](perf-pathology.md): same diagnosis-plan
  handoff shape, different atom set and evidence standards.
- Per-project artifacts live under `<project>/design/refactor/` and follow the
  [Design Corpus](../design-corpus.md) frontmatter conventions.

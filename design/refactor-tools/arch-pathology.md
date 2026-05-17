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
brief: "Diagnosis workflow that emits a reviewable, structured refactor plan-doc consumed by phase-decompose for execution. Smells are organized as universal invariants, paradigm-bounded invariants, and language-idiosyncratic invariants; v0 ships the Java language pack. Ships per-project example artifacts (detector packets, decomposer brofiles, layer-model template, plan-doc + sidecar schemas) that operators adapt under `<project>/design/refactor/`."
---

# Architecture Pathology

This design proposes an upstream diagnosis workflow that produces a reviewable
refactor plan-doc, which the existing phase-decompose dispatch lane then
executes. The pathologist diagnoses. Phase-decompose operates. The plan-doc is
the contract between them.

The deliverable is a set of example artifacts — detector packets, decomposer
brofiles, a layer-model template, plan-doc and sidecar schemas, a glue
workflow — that operators copy into a target project and adapt. Nothing in
this design ships as a generic auto-fit system. The per-project layer model
and detector tuning are part of the adoption work.

## Motivation

Hand-auditing a codebase for cross-layer coupling, god units, dead exports,
and similar structural smells is slow, lossy, and produces no durable record.
The existing refactor-tools surface ([AST-Assisted Refactor
Mechanization](ast-refactor-mechanization.md), [Refactor Compound
Runs](refactor-compound-runs.md), [Refactor Agents](refactor-agents.md))
covers the *execution* side once a plan exists. The
[phase-decompose](../orchestration/phase-decompose/) dispatch lane covers
bounded supervised execution from an implementation document. The gap is
upstream: there is no workflow that turns "this codebase has architectural
problems" into "here is a bounded, reviewable plan with measurable acceptance
criteria."

Architecture pathology fills that gap by composing existing primitives:
corpus-pathfinder for scouting, a parallel detector battery for evidence
collection, an ensemble-validate phase reusing the decomposer-panel pattern,
and a final plan-doc emission step that produces an artifact phase-decompose
already knows how to consume.

## Scope and non-goals

In scope:

- Mechanized detection of a closed set of v0 architecture smells (layer
  violation, dead export, god unit, runtime type discrimination, global
  state escape, scope mismatch), split across universal, paradigm-bounded,
  and language-idiosyncratic tiers (see [Smell
  taxonomy](#smell-taxonomy-universal-paradigm-bounded-language-idiosyncratic)).
- Validation, clustering, ordering, and bounding of findings by an ensemble
  decomposer panel.
- Emission of a structured plan-doc (markdown + frontmatter + sidecar findings
  YAML) that phase-decompose-main-edit consumes natively.
- Durable per-project history via git on the plan-doc artifacts and
  deterministic finding IDs.

Out of scope for v0:

- Auto-execution. The workflow stops at plan emission; operator review is
  mandatory before dispatch.
- A `Finding` graph entity type. Deterministic IDs plus git history plus
  Obsidian crosslinks cover every load-bearing v0 use case. Graph entities
  may become useful at org-scale / cross-repo rollup, addressed in [Future
  Work](#future-work).
- New refactor transform atoms. The pathologist proposes remediations that
  map to transforms the existing refactor surface either supports or doesn't.
  Unsupported transforms are emitted as advisory plan items, not failures.
- Continuous-CI gating. Running the detector battery as a build gate is an
  obvious next step but invites the precision-collapse failure mode common to
  lint-based gates. Defer.

## Architectural shape

Two workflows and one artifact contract:

```
arch-triage workflow
  inlet phase ......... corpus-pathfinder scouts, layer-model resolve,
                         prior-findings consultation
  detector phase ...... parallel fan-out, one brofile per smell kind,
                         each emits Finding records to the whiteboard
  ensemble phase ...... decomposer-panel validates evidence, clusters
                         correlated findings, topologically orders by
                         blocked_by, bounds slice size, cuts to top-N,
                         writes per-slice acceptance criteria
  emit phase .......... renders plan-doc markdown + writes findings sidecar
                         to <project>/design/refactor/plans/<plan-name>/

[operator review gate — reviewed_by + approved_at frontmatter fields]

phase-decompose-main-edit (existing, unmodified)
  consumes plan-doc as phase_doc_text, AB-PN criteria as acceptance_criteria,
  runs supervised-impl-edit per slice, returns satisfied | work_remains |
  untenable
```

The pathologist does not own execution. The operator does not bypass review.
The plan-doc is the only thing that crosses the gate.

## Artifact contract

### Plan-doc

Path: `<project>/design/refactor/plans/<plan-slug>.md`

Frontmatter:

```yaml
---
title: "Refactor Plan: <scope> — <date>"
kind: refactor-plan
lifecycle: proposed       # → in-flight → partial → satisfied → archived
corpus: <project>-refactor
topic:
  - refactor-plan
  - architecture
tags:
  - refactor-plan
  - pathology-output
date: <YYYY-MM-DD>
baseline_commit: <full-sha>
reviewed_by: null         # populated during operator review
approved_at: null
brief: "<one-line>"
related:
  - "[[Architectural Layer Model — <project>]]"
supersedes: []
superseded_by: []
findings_sidecar: "<plan-slug>.findings.yaml"
---
```

Body: prose summary, per-finding sections with `FND-xxxxxxxx` Obsidian-friendly
anchors, a Deferred section, an Out-of-scope section, a Crosslinks section.
The body is the human-review surface; the machine contract lives in the
sidecar.

### Findings sidecar

Path: `<project>/design/refactor/plans/<plan-slug>.findings.yaml`

```yaml
plan_slug: <slug>
baseline_commit: <full-sha>
layer_model: <path-relative-to-project>
findings:
  - id: FND-a3f12c4d
    smell_kind: layer_violation        # closed enum, see Detector Taxonomy
    locus:
      file: webapp/src/.../UserAdmin.java
      lines: "1-513"
    evidence:
      imports:
        - com.vaadin.flow.component.UI
        - com.vaadin.flow.server.VaadinSession
      graph_path: ["backend.admin.UserAdmin", "com.vaadin.flow.server.VaadinSession"]
      ast_pattern: import_crosses_layer
    measurements:
      confidence: 0.95
      blast_radius_direct_callers: 23
      blast_radius_packages_crossed: 4
      local_norm: 12.0  # ratio over package median
    blocked_by: []
    corroborates: [FND-b8e201a7]
    proposed_remediation:
      transform_kind: extract_servlet_helpers
      sketch: "Move VaadinSession-touching methods to a ui-layer adapter."
      atom_available: true
    acceptance:
      detector: layer_violation
      scope: webapp/src/.../UserAdmin.java
      predicate: zero_findings
      compile_required: true
      tests: [UserAdminTest, LoginFlowIntegrationTest]
deferred:
  - id: FND-c9d503f1
    cut_reason: "blast_radius above threshold; recommend next plan"
```

### Finding ID scheme

```
FND-{first-8-hex(sha256(smell_kind || normalized_locus || canonical_evidence))}
```

Same smell at same locus regenerates the same ID across runs. Dedup is record
equality on `id`. Evidence-hash inclusion means small evidence shifts produce
new IDs — intentional. The old ID can still be looked up via
`git log -S "FND-xxxxxxxx" design/refactor/` for longitudinal record.

### Layer model

Path: `<project>/design/refactor/layer-model.md`

```yaml
---
title: "Architectural Layer Model — <project>"
kind: layer-model
lifecycle: proposed | accepted | archived
corpus: <project>-refactor
topic: [refactor-plan, architecture]
date: <YYYY-MM-DD>
brief: "<one-line>"
layers:
  - name: ui
    packages: ["com.example.ui.**"]
  - name: backend
    packages: ["com.example.backend.**"]
rules:
  - id: LR-001
    name: backend_must_not_import_ui
    rule: "backend.* must not import (vaadin.* | ui.*)"
    severity: error
  - id: LR-002
    name: no_static_injector_outside_bootstrap
    rule: "GuiceVaadinServlet.injector accessible only under bootstrap.*"
    severity: error
---
```

Detectors read rules from frontmatter, not prose. The body explains intent and
trade-offs for human readers.

## Smell taxonomy: universal, paradigm-bounded, language-idiosyncratic

Smells fall into three tiers by how they port across languages. The
distinction is load-bearing: it determines which packets ship with the
universal contracts, which ride with a paradigm pack, and which exist only
inside one language pack.

**Universal invariants** apply in any reasonable language. The concept ports
unchanged; the detection backend is local. Every language has some notion of
dependency edges, declared visibility, units that do too much, and global
state escaping its bootstrap.

**Paradigm-bounded invariants** apply wherever a paradigm exists. Outside the
paradigm, the smell either does not exist or has a structurally similar
analogue under a different name. Examples: scoped dependency injection
(Spring, Guice, Dagger, Microsoft.Extensions.DI); nominal-type discrimination
(`switch (Class<?>)`, `instanceof` chains); async/await concurrency;
message-passing concurrency.

**Language idiosyncrasies** are only meaningful in one language because the
misused feature is exclusive to that language. These don't port — each is
authored fresh per language.

The taxonomy maps onto the existing per-language refactor-tools clusters
(`refactor-tools/rust/`, `refactor-tools/java/`, `refactor-tools/elixir/`,
`refactor-tools/csharp/`) without restructuring either side. A pathology
language pack pairs with its refactor-tools language pack: same scope
boundary, same operator workflow, same install pattern.

### v0 Tier-1 catalog

| Smell kind | Tier | Notes |
|---|---|---|
| `layer_violation` | Universal | Detection per-language: Java `import` edges, Elixir `alias`/`import`, C# `using`, Rust `use`. Contract: directed dependency edge crosses a boundary declared in the project layer model |
| `dead_export` | Universal | "Export" specializes per language (`public`/`protected` in Java/C#, `def` in Elixir, `pub`/`pub(crate)` in Rust). Contract: declared visibility exceeds actual usage (≤1 external referent) |
| `god_unit` | Universal | The unit specializes: `god_class` in Java/C#, `god_module` in Elixir, `god_package` in Go. Contract: fan-in × fan-out anomalous vs package-local norm |
| `runtime_type_discrimination` | Paradigm-bounded (nominal-dispatch) | Dispatching on runtime type/identity via comparison instead of polymorphism. Java/C# fit (`switch (Class<?>)`, `instanceof` chains, type-switch on `Type`). Does not apply where pattern matching is the idiomatic dispatch primitive (Elixir, ML-family) |
| `global_state_escape` | Universal | Service-locator or global-mutable-state access outside declared bootstrap. Specializes: `GuiceVaadinServlet.injector` (Java/Guice), `Application.get_env/2` in hot paths (Elixir), `ServiceLocator.Current` (C#), `lazy_static!` accessor wrappers (Rust) |
| `scope_mismatch` | Paradigm-bounded (scoped-di) | Narrower-scoped resource bound into wider-scoped consumer. Requires a scoped DI container (Spring, Guice, Dagger, Microsoft.Extensions.DI). Analogue in Elixir is process-lifetime mismatch (long-lived `GenServer` holding a ref to a short-lived ETS table or supervised child); structurally similar, ships as a separate Elixir-pack smell under a different name |

v0 ships only the Java language pack. The universal and paradigm contracts
exist as ports for future Elixir, C#, and Rust packs.

### Per-language pack structure

Each language pack implements the universal and applicable paradigm contracts,
plus a set of language-idiosyncratic detectors. None of the idiosyncratic
detectors below ship at v0; they are illustrations of what a pack will accrue
in its follow-up design.

- **Java**: raw types and unchecked generics, `synchronized` on `this` or
  class literals, mutable non-final static fields, reflection bypassing
  `private` access, `equals`/`hashCode` contract violations.
- **C#**: `async void` outside event handlers, `Task.Result`/`.Wait()`
  deadlock patterns, `IDisposable` not implemented for resource-owning types,
  `dynamic` used to bypass the type system, configure-await missing in
  library code.
- **Elixir**: blocking `GenServer.call` inside supervisor init, unhandled
  `:DOWN`/`:EXIT` messages, atom DoS (dynamic atom creation from user input),
  process dictionary use, side effects inside lazy `Enum` chains, missing
  `@spec` on public functions in typed boundaries.
- **Rust**: `unsafe` blocks without `// SAFETY:` comments, `Box<dyn Trait>`
  where generics suffice, `unwrap`/`expect` outside test code, lifetime
  elision that hides aliasing, `Arc<Mutex<...>>` where channel-based
  ownership would be clearer.

### Packet schema

Each detector ships as a packet under
`system-defaults/agentic-corpus/packets/arch-pathology/{universal,paradigm/<paradigm>,languages/<lang>}/`.
The packet shape carries tier metadata:

```yaml
smell_kind: layer_violation
tier: universal                       # or "paradigm:scoped-di" or "language:java"
invariant: "directed dependency edge crosses a boundary declared in the project layer model"
language: null                        # null for universal contracts; set for language backends
implements_universal: null            # set on language packets that implement a universal contract
paradigm_prerequisites: []            # populated when tier=paradigm
detection:                            # only on language-tier packets
  backend: jdtls | roslyn | elixir-lsp | tree-sitter
  pattern: <backend-specific>
evidence_schema: {...}                # what lands in findings[].evidence
remediation_template:
  transform_kind: <atom-name or :manual>
confidence_prior: 0.0                 # per-detector baseline; corroboration bumps in ensemble
```

Universal-tier packets carry the invariant statement, evidence schema, and
remediation template with `language: null` — they are contracts. Language-tier
packets implement contracts by referencing `implements_universal:` and
supplying the detection backend. Two-key composition: universal smell ×
language backend = a runnable detector.

Higher tiers (Tier 2 graph-stats with calibration, Tier 3 git-churn-weighted
hotspots, Tier 4 LLM-hypothesis semantic smells) are deferred to follow-up
designs.

## Workflow composition

The `arch-triage` workflow is one new file. Inlet, ensemble, and emit phases
each reuse existing primitives:

- Inlet: dispatches `corpus-pathfinder` (existing agent) for operator-named
  hotspots; calls `arch_graph_snapshot` (see [Open
  questions](#open-questions)) to materialize the typed reference graph;
  resolves the layer model from `<project>/design/refactor/layer-model.md`;
  walks prior plan-docs for `FND-*` IDs to suppress already-accepted tech debt.
- Detector phase: parallel fan-out to brofiles in the
  `arch-detector-panel` team (one brofile per Tier-1 smell). Each brofile
  posts findings to a shared whiteboard with packet-prescribed evidence.
- Ensemble phase: reuses the decomposer-panel and recompose-council patterns
  from [phase-decompose](../orchestration/phase-decompose/). The ensemble's
  job here is narrower: validate, cluster, order, bound, cut, write
  acceptance. It does *not* decompose work units — that is phase-decompose's
  job once the plan-doc is consumed.
- Emit phase: a small renderer node writes plan-doc.md and the findings
  sidecar to `<project>/design/refactor/plans/`.

The `phase-decompose-main-edit` workflow is unmodified. It receives the
plan-doc as `phase_doc_text` and the `findings[].acceptance` entries as
`acceptance_criteria`.

## Per-project adoption

Operators copy example artifacts into their project and adapt:

1. **Create the project refactor hub.** Copy
   `examples/arch-pathology/refactor-hub.md` to
   `<project>/design/refactor/refactor.md`. Adjust the `corpus:` field and
   set the `languages:` array (e.g. `[java]`, `[csharp]`, `[elixir, c]` for a
   NIF-heavy Elixir project). The install step is language-aware and pulls
   only the relevant language packs.
2. **Write the layer model.** Copy
   `examples/arch-pathology/layer-model.template.md` to
   `<project>/design/refactor/layer-model.md`. Fill in `layers:` packages and
   `rules:` entries. This is the load-bearing input — bad model, noisy output.
3. **Install pathology artifacts.** Run the install block from
   `examples/arch-pathology/install.md`. It installs the universal contracts,
   the paradigm packs your stack actually uses, and the language packs
   declared in `languages:`. Mirrors the [phase-decompose install
   pattern](../orchestration/phase-decompose/install.md).
4. **Dispatch the workflow.** `bro_orchestrate_run` with `workflow_id =
   "arch-triage"` and `initial_vars: { project_dir, layer_model_path,
   scope_filter? }`. The workflow emits the plan-doc and exits.
5. **Review the plan-doc.** Read `<project>/design/refactor/plans/<plan-slug>.md`.
   Edit out slices, adjust ordering, change `lifecycle` to `in-flight`,
   populate `reviewed_by` and `approved_at` in frontmatter. Commit.
6. **Dispatch execution.** Standard `phase-decompose-main-edit` invocation
   with `phase_doc_path` pointing at the reviewed plan-doc. PD reads the
   findings sidecar via the path declared in frontmatter and converts
   `findings[].acceptance` to its own `acceptance_criteria`.
7. **Update the plan-doc.** When PD returns `satisfied`, flip `lifecycle:
   satisfied`. When it returns `work_remains`, the operator decides whether
   to rerun pathology against the new baseline or re-dispatch PD with a
   higher epoch ceiling.

## Example artifacts shipped

Under `system-defaults/agentic-corpus/`:

```
packets/arch-pathology/
  universal/
    layer-violation.json        # invariant + evidence + remediation contract
    dead-export.json
    god-unit.json
    global-state-escape.json
  paradigm/
    scoped-di/
      scope-mismatch.json
    nominal-dispatch/
      runtime-type-discrimination.json
  languages/
    java/
      layer-violation.json      # jdtls backend; implements_universal=layer_violation
      dead-export.json
      god-unit.json              # specializes to god_class
      global-state-escape.json   # GuiceVaadinServlet.injector, static mutable fields
      runtime-type-discrimination.json  # switch on Class<?>, instanceof chains
      scope-mismatch.json        # Guice/Spring/CDI scope-flow analysis
      idiosyncratic/             # empty at v0; reserved for follow-up Java pack
    # csharp/ elixir/ rust/ — follow-up designs

brofiles/arch-pathology/
  detector-layering.json         # universal: layer_violation
  detector-deadcode.json         # universal: dead_export
  detector-godunit.json          # universal: god_unit
  detector-globalstate.json      # universal: global_state_escape
  detector-typediscriminate.json # paradigm: runtime_type_discrimination
  detector-scopemismatch.json    # paradigm: scope_mismatch
  ensemble-facilitator.json
  ensemble-acceptance.json

teams/arch-pathology/
  arch-detector-panel.json       # composes detector brofiles into the panel

workflows/arch-pathology/
  arch-triage.json
```

Detector brofiles are per-contract, not per-language. Each brofile loads the
relevant language-pack packets for the project's declared `languages:`. Adding
a new language pack adds packets, not brofiles.

Under `examples/arch-pathology/` (operator-facing templates):

```
refactor-hub.md                 # project-side design/refactor/refactor.md template
layer-model.template.md         # project-side layer-model.md template
plan-doc.template.md            # plan-doc structure illustration
findings.template.yaml          # sidecar schema in template form
install.md                      # the install command block, language-aware
edit-implementer-refactor.json  # the brofile variant for execution side
language-pack-roadmap.md        # placeholder hub for future C#/Elixir/Rust packs
```

The `edit-implementer-refactor` brofile lives with the examples rather than
with system defaults because it composes the existing `edit-implementer` with
project-specific refactor atom availability. Each project sets its own atom
allowlist when installing.

## Risks and design choices

**Plan-doc drift vs. HEAD.** The plan is a snapshot against
`baseline_commit`. PD's discovery phase must check that HEAD has not moved
beyond the baseline in ways that touch any AB-PN locus, and fail-fast if it
has. The mitigation is mechanical (a discovery-phase check), but the workflow
must surface it as an explicit refuse-to-start rather than a silent stale-run.

**Operator review skip.** The entire architecture assumes the operator reads
the plan-doc before triggering PD. PD's existing convention of accepting
`phase_doc_path` makes it easy to bypass review entirely. Mitigation: PD's
discovery phase, when consuming a `kind: refactor-plan` document, requires
`reviewed_by` and `approved_at` to be populated and refuses otherwise. This
puts the gate in the consuming workflow, not the emitting one — defense in
depth.

**Refactor atom capability gaps.** Tier-1 detectors propose transforms that
may not have backing atoms. The plan-doc must mark each finding with
`proposed_remediation.atom_available: true | false`. Findings with
`atom_available: false` are emitted as advisory plan sections that PD's
edit-implementer handles as manual edits with extra review. The first
real-codebase run is expected to expose 20–40% advisory findings; this is
acceptable for v0 and feeds back into refactor-tools work.

**False-positive tax.** Six detectors firing in parallel will produce
overlapping findings on the same loci. The ensemble's clustering step is
load-bearing: poorly tuned, the operator drowns. Initial corroboration
threshold should be conservative (require ≥2 detectors or ≥0.85 confidence to
include in the plan; everything else goes to `deferred`). Tune from observed
operator review experience, not from theory.

**Layer-model bootstrap cost.** Writing a usable layer model is meaningful
operator work on a real codebase. The example template ships with prose
explaining how to extract one from package conventions and existing
`package-info.java` (or equivalent) files, but the work doesn't disappear.
Without a reasonable layer model, every detector regresses to noise.

**Language pack coverage.** v0 ships only the Java language pack. Universal
and paradigm-bounded invariants exist as language-independent contracts under
`packets/arch-pathology/universal/` and `packets/arch-pathology/paradigm/`;
their detection backends are implemented under
`packets/arch-pathology/languages/java/`. Adding an Elixir, C#, or Rust pack
is a sibling design: implement the universal contracts with the new
language's detection backend, implement applicable paradigm contracts, and
author any language-idiosyncratic detectors specific to that pack. The
taxonomy makes the porting work bounded — the contracts are stable, the
backends vary.

## Notes on rejected alternatives

A `Finding` graph entity type with `LOCATED_IN`, `CORROBORATES`, `BLOCKED_BY`,
`SUPERSEDED_BY` edges was considered and rejected for v0. Deterministic IDs
plus `git log -S` plus Obsidian wikilink backlinks cover every load-bearing
use case (dedup across runs, longitudinal record, cross-finding references,
post-PD resolution tracking). The graph approach wins only at cross-repo
rollup — see [Future Work](#future-work).

A single-file plan-doc with embedded structured findings (YAML fenced
codeblock in the body) was considered. The companion sidecar pattern was
chosen because no other design doc in the corpus uses embedded full-payload
structured blocks; convention convergence outweighs the lifecycle-coupling
argument.

Auto-dispatch from pathologist directly to PD without an operator gate was
considered and rejected. The architecture only works if review happens.
Skipping it collapses to "auto-fix-everything," which has a long history of
encoding bad taste into codebases faster than humans would.

## Future work

- **Additional language packs**: Elixir, C#, and Rust packs. Each implements
  the universal contracts plus applicable paradigm contracts, plus a
  language-idiosyncratic catalog. Pairs with the existing per-language
  refactor-tools clusters under `refactor-tools/<language>/`.
- **Additional paradigm packs**: detection for paradigms not covered at v0 —
  message-passing concurrency (BEAM, Akka), async/await (C#/TypeScript/Rust),
  ownership-and-borrowing (Rust). Each paradigm pack ships its own
  paradigm-bounded smell contracts.
- **Tier-2 detectors**: graph-stats baselines that require a calibration
  pass (per-package medians for fan-in / fan-out / cyclomatic).
- **Tier-3 detectors**: git-churn-weighted hotspots; shotgun-surgery
  detection from co-change history.
- **Tier-4 detectors**: LLM-assisted semantic smells (misnamed modules,
  conceptual duplication, boundary-via-type). Always flagged as hypotheses
  with mandatory operator triage.
- **Cross-repo rollup**: an indexer that walks every registered project's
  `design/refactor/` directory and answers "where are our worst god-classes
  org-wide?" At that point a graph-backed index over the markdown corpus
  earns its keep — but it would be an *index*, not the canonical store.
  Plan-docs remain authoritative.
- **CI integration**: read-only pathology run on PRs that reports finding
  delta vs. main. Conservative threshold; advisory only; never blocks.
- **Layer-model inference**: a first-run helper that proposes a layer model
  from package conventions, import diversity, and existing `package-info`
  files. Output is reviewed and edited by the operator, not consumed
  directly.

## Relationship to existing designs

- Built on top of [Phase-Decompose
  Dispatch](../orchestration/phase-decompose/) — consumes the existing
  `main-edit` lane unchanged.
- Upstream of [AST-Assisted Refactor
  Mechanization](ast-refactor-mechanization.md) and [Refactor
  Agents](refactor-agents.md) — the pathologist proposes; those execute.
- Sibling to [Refactor Compound Runs](refactor-compound-runs.md) — a
  pathology-derived plan typically runs as a compound execution.
- Pathology language packs (Java at v0; Elixir, C#, Rust as they land) pair
  with the per-language [Refactor Tools](refactor-tools.md) clusters under
  `refactor-tools/<language>/`. Same scope boundary, same operator workflow,
  same install pattern. The universal and paradigm contracts are shared
  across packs; the detection backends and idiosyncratic catalogs live under
  each language's directory.
- Per-project artifacts live under `<project>/design/refactor/` and follow
  the [Design Corpus](../design-corpus.md) frontmatter conventions
  (lowercase-with-dashes filenames, descriptive hub note names, lifecycle
  metadata, Obsidian-compatible wikilinks).

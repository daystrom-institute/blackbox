---
title: "Rust Architecture Pathology"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - refactor-tools
  - rust
  - pathology
tags:
  - refactor-tools
  - rust
  - architecture
  - pathology
date: 2026-05-20
updated: 2026-05-30
status: "partial — full V0 shipped (arch-pathology-rust workflow + 12 V0 atoms + brofile + operator docs); post-v0 deferred atoms remain"
brief: "Rust pathology workflow: identify Rust-specific bad-code architecture, then emit correction plans that map to Rust refactor atoms, cargo validation, and PD remediation."
---

# Rust Architecture Pathology

## Implementation status (2026-05-30)

**Partial — full V0 is shipped and installable.** Grounded against artifacts as
of 2026-05-30:

- Workflow `arch-pathology-rust` —
  `system-defaults/workflows/refactor/arch-pathology-rust.json` (6 nodes + actors).
- Brofile `rust-architecture-pathologist` —
  `system-defaults/brofiles/refactor/rust-architecture-pathologist.json`.
- All **12 V0 diagnosis atoms** shipped under
  `system-defaults/atoms/refactor/rust-architecture-*.json` (matches §"V0 Rust
  pathology atoms" exactly).
- Operator runbook: `docs/pathology-dispatch.md` (install + dispatch) and
  `docs/reference-implementations.md`.

Remaining: the §"Rejected or deferred from v0" / §"Future work" atoms, to be
revisited after real pathology runs. A successful end-to-end live run is not
recorded in-doc — kept at `partial` rather than archived for that reason. The
sibling **[perf-pathology](../perf-pathology.md)** is the unbuilt counterpart;
this workflow is its proven template.

Rust architecture pathology is the Rust language pack for
[Architecture Pathology](../arch-pathology.md). It keeps the same forensic
workflow: spend LLM time only where static tools cannot make the diagnosis,
triangulate evidence, and emit a reviewed correction plan for later
`phase-decompose-main-edit` remediation. It does not edit code.

The Rust version differs from the Java v0 shape in one load-bearing way:
Rust's compiler and rust-analyzer are stronger authorities than most Java
frameworks, but they are deliberately narrow. They prove borrow, type,
visibility, and import facts. They do not decide that a 40-method impl has
three conceptual owners, that a trait is pretending to be a service locator,
that an error type has become an architectural dumping ground, or that feature
gates encode an accidental product matrix. Rust pathology owns those judgments.

After operator review, the correction plan is handed to
[Phase-Decomposer Dispatch](../../../docs/pd-dispatch.md) as normal
`phase_doc_text` plus explicit `acceptance_criteria`.

## Relationship to the Java workflow

The Java workflow is the template, not the atom catalog. Reuse these invariants
unchanged:

- Pathology is upstream of refactor execution and stops at a correction plan.
- Cheap survey comes first; LLM diagnosis is bounded to likely hotspots.
- Whiteboard review challenges, merges, or rejects diagnosis candidates before
  the plan is durable.
- Findings that SAST can make are rejected as pathology atoms.
- Multiple atom signals on one locus become one diagnosis, not a smell dump.
- The output is a PD-ready correction plan with evidence, ordered slices, and
  acceptance criteria.

Rust-specific differences:

- Cargo, rustc JSON diagnostics, rust-analyzer, clippy, `cargo metadata`, and
  feature-matrix checks are cheap measurement authorities.
- `semantic_status` matters. `syntax_only` and `indexed_hints` are evidence,
  not proof. `lsp_verified` and compiler diagnostics are stronger but still do
  not infer architectural intent.
- Macro expansion and proc-macro generated code are explicit uncertainty
  zones. Pathology may recommend mechanical follow-up, but it must not pretend
  tree-sitter saw generated behavior.
- Authority promotion is explicit. A claim grounded in `indexed_hints` may rise
  to `lsp_verified` or compiler-confirmed only when a follow-up measurement
  consumes that authority. Plans must record both the current and required
  grades per claim before remediation.
- Public API and representation opt-outs are operator authority. Pathology may
  recommend public-surface or `#[repr]` changes, but the correction plan must
  surface them; atoms must not set `acknowledge_public_api_change` or
  `acknowledge_repr` on the operator's behalf.

## Scope and non-goals

In scope:

- Rust crates and Cargo workspaces, including bin/lib splits, feature-gated
  code, async services, CLI/provider dispatch code, error plumbing, test-heavy
  modules, and macro-heavy regions where uncertainty is explicitly recorded.
- Whole-project damage assessment with bounded hotspot selection.
- Shared measurement over symbols, impl/method graphs, fields, imports, calls,
  tests, cargo metadata, feature flags, public surfaces, git history, and
  Blackbox transcript history.
- Rust-specific pathology atoms whose diagnoses require semantic judgment
  beyond rustc, clippy, rust-analyzer, or simple metrics.
- Correction plans that map remediation slices to existing Rust atoms where
  possible, and to named G-series gaps or PD-manual slices where not.

Out of scope:

- Auto-execution.
- Reimplementing clippy, rustfmt, cargo-semver-checks, dead-code analysis, or
  clone detection.
- Treating every compiler or clippy diagnostic as architecture pathology.
- Macro-expansion-aware claims unless a measurement pass explicitly used a
  macro expansion source such as `cargo expand`.
- Local idiom cleanup such as import ordering, small rename hygiene, or
  one-function extraction unless it changes ownership, seams, lifecycle, or
  contracts.

## SAST and compiler gate

Every Rust pathology candidate must pass this gate:

1. **Static tools cannot answer the architectural question.** rustc, clippy,
   rust-analyzer, cargo metadata, and metrics may provide measurements, but the
   pathology claim must require ownership or design judgment.
2. **The LLM judgment changes the diagnosis.** "This impl has 48 methods" is a
   metric. "This impl has transport setup, provider catalog policy, session
   discovery, and event parsing collapsed behind one receiver" is pathology.
3. **The remediation is architectural.** It changes module ownership, state
   ownership, trait boundaries, error contracts, feature boundaries, async
   lifecycle, public API shape, or test seams.
4. **Compiler authority is preserved.** If the claim depends on type, borrow,
   trait, import, or public API facts, the plan says which facts are
   `syntax_only`, `indexed_hints`, `lsp_verified`, or compiler-confirmed.
5. **Question-mark and conversion claims defer to the compiler.** Any claim
   that depends on which `From<X> for E` impls exist, on `?`-site
   convertibility, or on borrow-checker outcomes is recorded as uncertainty
   and routed to compiler-backed validation during remediation.
6. **Operator-authority opt-outs are not pathology authority.** A plan that
   depends on `acknowledge_repr` or `acknowledge_public_api_change` must surface
   those flags as operator gates per RX-V1, not bury them inside a slice.

Candidates that fail the gate are normal refactor work, lint cleanup, or
compiler repair. They may appear in a remediation slice as validation or
cleanup, but not as pathology diagnoses.

## Workflow shape

```text
rust-arch-pathology
  input ............... project_dir, scope_filter, optional operator hints,
                         optional crate/workspace notes, target context window
  cheap survey ........ cargo metadata, module tree, public surfaces
                         (rust_public_api_guard baseline), symbols, impl
                         graphs, field access, call graph, tests, feature
                         flags, macro density, git/transcript history,
                         existing refactor plans
  hypothesis loop ..... pathologist selects Rust atoms, asks for targeted
                         measurements, posts diagnosis candidates tagged with
                         current authority grade, and requests authority
                         promotion when evidence is weak
  whiteboard review ... specialists challenge/corroborate candidates, merge
                         overlapping claims, reject compiler/lint-shaped
                         findings, and choose remediation slices
                         weak or conflicting evidence loops back to targeted
                         measurement
  emit ................ correction plan markdown with evidence, remediation
                         slices, atom mapping, authority grades, and PD-ready
                         acceptance criteria

[operator review]

phase-decompose-main-edit
  receives the reviewed plan as phase_doc_text and acceptance criteria as
  normal initial_vars.acceptance_criteria
```

This is not a static fan-out across every atom. The workflow starts from the
operator's suspicion or the cheap survey, runs the few atoms that can answer the
next diagnostic question, and loops until the surviving plan is worth editing.
For macro-dense loci, the loop must either run a macro-expansion measurement
pass, bound the claim to non-macro regions, or stamp explicit "macro expansion
not inspected" uncertainty.

## Whole-project damage assessment

When the operator asks for whole-project damage, the Rust workflow follows the
Java triage discipline but uses Rust pressure signals:

1. **Cheap survey.** Build a pressure map from module size, impl size,
   method-field-call graphs, public API surfaces, feature flags, cfg density,
   error-type fan-in, `unsafe` boundaries, async/task spawning, test islands,
   cargo workspace shape, git churn, failed compile-fix loops, transcript pain,
   and existing refactor reports.
2. **Hotspot selection.** Rank loci by corroborated pressure: repeated
   operator pain, high-churn god impls, dense shared mutable state, public API
   friction, feature-matrix risk, macro opacity, test setup pain, and boundary
   overlap. Pick a bounded top set.
3. **Focused atom runs.** Use only atoms that answer the next question. A god
   impl first needs partition/state evidence; an error hotspot needs error
   contract analysis; a feature hotspot needs cfg and cargo metadata evidence.
4. **Cluster synthesis.** Merge multiple atom signals into damage clusters.
   A method graph, state-field cluster, and transcript complaint around the
   same impl become one diagnosis.
5. **Correction plan.** Emit the few clusters worth acting on first, with
   uncertainty, authority grades, atom mapping, remediation order, and deferred
   candidates.

The output is not "300 Rust smells". It should be closer to:

```text
The worst Rust architectural damage is concentrated in:

1. Provider orchestration collapsed into one enum/impl instead of spec + driver
   modules.
2. BlackboxServer state and tool routing share one receiver and force
   cross-domain field access.
3. Error conversion and public API shape make compile-fix loops hide ownership
   drift.

Here is the evidence, what to fix first, which atoms can execute each slice,
what requires new primitives, and what acceptance criteria PD should enforce.
```

## Shared measurement substrate

The cheap substrate should prefer structured authority before provider context:

- `bbox_code_symbols` and `bbox_refactor_status` for Rust symbol inventory,
  item ranges, impls, fields, attrs, tests, and module declarations.
- `rust_impl_partition_analysis` / `rust-impl-partition-graph` for method,
  field, and call graph evidence inside impl blocks.
- `extract_rust_impl_methods(deep_analysis=true)` for captured fields,
  unresolved callbacks, inherited generics/bounds, and extraction hazards.
- Rust-analyzer-backed plan kinds (`rust_lsp_rename`, `rust_organize_imports`,
  `rust_ra_move_item_to_module`, `rust_ra_classify_callbacks`) when LSP
  authority is needed; these fail closed on `error.lsp_unavailable` per RX-V3.
  Pathology must not paper over an LSP outage with `syntax_only` fallback
  claims.
- `rust_public_api_guard` for advisory public-surface impact before any plan
  that changes `pub` items, re-exports, trait signatures, or error types.
  The Rust guard uses `warning` for the practical caution band.
- `cargo check --message-format=json`, `cargo test`, and targeted feature
  matrix checks for compiler-confirmed facts.
- `cargo clippy --message-format=json` for architecture-adjacent lint evidence
  such as `type_complexity`, `large_enum_variant`, or `module_inception`; these
  are inputs, not automatic pathology findings.
- `cargo metadata` / `Cargo.toml` parsing for crate boundaries, bin/lib
  structure, feature flags, optional dependencies, and workspace membership.
- Test attribution by syntactic calls, naming, `#[cfg(test)]` modules, and
  integration-test imports; `rust_test_attribution` (G18) is the gap-backed
  analysis primitive for this when a syntactic mapping is not enough.
- Git history for co-change, fix/revert loops, growth, and commit narrative.
- Blackbox transcripts, notes, work threads, and decisions for prior operator
  pain and abandoned refactor plans.

## V0 Rust pathology atoms

### 1. Impl Role Coherence

Diagnostic question: does this `impl` or module do what its name, receiver,
module path, traits, attrs, and tests claim it does?

Metrics can count methods and fan-out. They cannot decide that an impl named
around server orchestration also owns provider catalog policy, CLI argv
construction, MCP config generation, event parsing, and session discovery. This
atom classifies methods into responsibility families, compares them to the
declared role, and identifies role-shaped extraction slices.

Primary measurements: `rust-impl-partition-graph`,
`extract_rust_impl_methods(deep_analysis=true)`, method attrs, module path,
tests, and transcript history.

Correction-plan output: intended role, actual method clusters, foreign
clusters, cross-cluster callbacks, state captured by each cluster, and
candidate atoms: `rust-extract-impl-methods` for a single foreign method
cluster, `rust-split-god-impl` for reviewed multi-domain partitions,
`rust-extract-to-submodule` for top-level item clusters, or manual PD slices.

### 2. State Ownership Collapse

Diagnostic question: are unrelated responsibilities coupled because one struct
owns too many fields or because all behavior reaches through one receiver?

Rust makes state ownership explicit, but that can hide architectural collapse:
one struct becomes the only path to config, stores, caches, runtime handles,
provider metadata, and request state. The compiler confirms field access is
legal; it does not decide which state clusters belong together.

Primary measurements: field read/write clusters, constructor wiring,
`captured_self_fields`, `remaining_source_accessors`, borrow context,
interior-mutability calls, sync lock topology (`Arc<Mutex<_>>`,
`Rc<RefCell<_>>`, `RwLock<_>`), and co-change history.

Correction-plan output: field clusters, legitimate shared state to leave
alone, delegate/state structs to extract, initialization-order hazards,
`#[repr]` warnings, candidate use of `rust-state-extract`, and
`rust-split-god-impl` when state extraction and method repartitioning must be
sequenced together. Any `rust-state-extract` slice must surface
`acknowledge_repr` as an operator-authority gate whenever the source struct has
a non-default `#[repr(...)]`.

### 3. Construction Boundary Collapse

Diagnostic question: does one constructor, builder, or initialization function
cross domain boundaries without declaring them?

Rust has no DI container to blame, so wiring collapse often appears as a
constructor or builder that touches unrelated subsystems because they share a
crate address. This is distinct from state ownership: the pathology is the
integration seam, not only the fields being integrated.
Structural dependency graphs may signal this; the architectural claim is
whether the crossings were intentional and owned by the wiring function.

Primary measurements: constructor/builder call graph, module dependency graph,
method-field-call graphs when the builder lives on an impl, cargo target shape,
and git co-change between the constructor and unrelated modules.

Correction-plan output: boundary crossed by the wiring function, domains it
initializes, intended owner for each domain, and remediation such as per-domain
builders, a facade, `rust-split-god-impl` when the builder lives on a god impl,
or PD slices when no atom fits.

### 4. Trait Boundary Mismatch

Diagnostic question: is the trait boundary too broad, too narrow, pretending to
be a service locator, or missing where callers already depend on a behavior
surface?

The compiler can check object safety and trait impl completeness. It cannot
decide that a trait mixes command execution, config rendering, session lookup,
and event parsing, or that callers should depend on a smaller trait instead of
a concrete type.

Primary measurements: trait methods, impl methods, call sites, test mocks,
object-safety reports, public API guard, generic-parameter bloat that papers
over a missing trait, and caller-migration risk.

Correction-plan output: intended trait role, method groups, object-safety
constraints, caller migration risk, and public API impact. `rust-trait-from-impl`
applies only to the missing-trait-surface sub-shape. Trait-too-broad or
service-locator traits need `rust-split-trait` (G19, gap-backed) and likely
`rust_find_references` (G10) for impact analysis; until those ship, remediation
is PD-manual with the gap IDs surfaced.

### 5. Module Topology Drift

Diagnostic question: does the module tree still represent ownership, or is it
an accident of where code first landed?

Rust modules are lightweight enough that files often grow around chronology
instead of concepts: `main.rs` carries library logic, `providers.rs` carries
both data catalog and behavior, test modules hide production seams, or `mod`
declarations encode no real boundary.

Primary measurements: module tree, `mod` declarations, public re-exports,
`use super::*` density, `crate::`/`super::` paths, bin/lib split, Cargo
targets, top-level dependency analysis, and optional external module-tree
reports such as `cargo modules structure` when available. Pathology consumes
module-structure tooling; it does not reimplement it.

Correction-plan output: current topology, intended topology, cross-boundary
paths, re-export/public API risk, and ordered slices using
`rust-extract-to-submodule`, `rust-bin-to-lib-migration`, or deferred module
tree primitives. Any `rust-bin-to-lib-migration` recommendation must surface
the known G22/root-alias caveat (`note-c699da56`): the underlying macro can
emit invalid `use mod <module>;` syntax when repairing unqualified call sites.

### 6. Error Contract Drift

Diagnostic question: have error types, `Result` signatures, and conversion
paths become an accidental architecture boundary?

rustc can show conversion failures and `?` incompatibilities. It cannot decide
whether an error enum is actionable for callers, whether variants expose the
wrong layer, or whether one broad error type is hiding ownership drift across
modules.

Primary measurements: error type definitions, functions returning the error,
`?` sites, construction forms, public API guard, downcast/downcast_ref sites,
tests asserting error behavior, and transcript pain.

Correction-plan output: current contract, caller needs, variants or new error
types, explicit construction mappings, public API impact, and possible
`rust-error-migrate` slices. Any `rust-error-migrate` slice must surface
`acknowledge_public_api_change` as an operator-authority gate per RX-V1.
Result-wrapping that needs `?` propagation across a call chain is gap-backed
G16 (`rust_wrap_return_in_result`).

### 7. Feature and Configuration Matrix Entanglement

Diagnostic question: do `#[cfg]`, feature flags, optional dependencies, and
Cargo targets encode product or platform boundaries in a way the module design
does not own?

Cargo can enumerate features, and rustc can compile selected configurations.
They do not decide that feature gates are scattered across unrelated modules,
that optional provider support leaks into core logic, or that a bin/lib split
is missing.

Primary measurements: `Cargo.toml` features and optional deps, `#[cfg]` sites,
target-specific modules, feature-matrix compile results, and public API deltas.

Correction-plan output: feature domains, scattered cfg sites, canonical owner,
validation matrix, and remediation slices. No mutating atom is shipped for cfg
attribute restructuring. Plans must label these slices `PD-manual, gap-backed
G15 (rust_add_cfg_attribute)`. `ensure_toml_table` can edit `Cargo.toml`
feature tables; cfg attribute insertion remains hand work until G15 ships.

### 8. Async and Runtime Lifecycle Capture

Diagnostic question: is short-lived or runtime-bound state captured into tasks,
closures, globals, or long-lived structs with the wrong lifecycle?

The borrow checker catches many local lifetime violations. It does not express
semantic runtime promises: cancellation, shutdown ordering, background task
ownership, handle lifetimes, lock discipline, channel ownership, and `Send` /
`Sync` boundaries introduced by architecture rather than syntax.

Primary measurements: `tokio::spawn` / task creation, channel creation,
runtime handles, `Arc<Mutex<_>>` / `RwLock` fields, static/global state,
shutdown code, tests, and compiler diagnostics from feature/default matrices.

Correction-plan output: captured resource, owner lifetime, task lifetime,
failure mode, and remediation such as explicit runtime owner, cancellation
token, channel boundary, state extraction, or adapter module. No shipped atom
executes runtime-lifecycle remediation directly. State relocation may chain
through `rust-state-extract`; cancellation, channel ownership, and task
lifetime are PD-manual.

### 9. Test-Implied Rust Architecture

Diagnostic question: what architecture do the tests wish the Rust code had?

The signal is in test pain: huge inline `#[cfg(test)] mod tests`, tests using
`super::*` to reach private internals, fixture construction that mirrors
production wiring, test-only feature flags, broad concrete types where a trait
seam is implied, and fragile compile-only tests around public API.

Primary measurements: inline test modules, integration tests, helper modules,
syntactic test attribution, fixture setup, public/private access patterns, and
co-change between tests and production.

Correction-plan output: test intent, workaround technique, missing production
seam, and remediation such as `rust-test-island-extract`, `rust-trait-from-impl`,
`rust-state-extract`, `rust-extract-impl-methods`, or module split. When test
to production attribution is the weak link, label it gap-backed G18.

### 10. Unsafe Contract Opacity

Diagnostic question: is `unsafe` being used as an architectural boundary
without a reviewable contract that callers can reason about?

The existence of `unsafe` is not pathology. Counting unsafe blocks is shared
measurement, not a diagnosis. The atom fires only when callers depend on an
undocumented invariant at the unsafe boundary.

Primary measurements: `unsafe` blocks/functions/traits, safety comments, tests
around invariants, public API guard, and call sites that rely on the invariant.

Correction-plan output: hidden contract, caller obligations, uncertainty, tests
that should lock the contract, and remediation such as boundary documentation,
adapter extraction, or safer abstraction. `rust_doc_harden` (G23) may become a
supporting documentation primitive, but no mutating safety-boundary atom ships.

### 11. Macro-Generated Contract Opacity

Diagnostic question: does macro-generated code hide ownership, lifecycle, or
public API that callers must manually reason about?

The existence of macros is not pathology. The pathology is when generated code
becomes the only place an architectural contract exists, or when proc-macro
attributes generate APIs that remediation would accidentally break.

Primary measurements: macro invocations, proc-macro attributes, generated APIs
if `cargo expand` is explicitly used, tests around generated behavior, and
public API guard.

Correction-plan output: generated contract, caller obligations, whether macro
expansion was inspected, public API risk, and remediation such as adapter
extraction or generated-code isolation. Claims must state when macro expansion
was not inspected.

### 12. Transcript-Anchored Rust Architectural Pressure

Diagnostic question: where has operator or agent history already identified
Rust architectural pain, and does current code confirm it?

Blackbox has transcripts, notes, work threads, decisions, refactor plans, and
git provenance. Rust pathology checks whether current code still matches the
complaint and whether the architectural pressure has grown since the prior pain:
repeated complaints about a file, failed refactor attempts, public API opt-out
debates, compile-fix churn, or abandoned module-split plans. Narrative alone is
insufficient.

Primary measurements: `bbox_search` for operator complaints, `bbox_notes` for
agent-side pain, `bbox_thread_list` for abandoned work threads,
`bbox_hybrid_search` for related design docs and decisions, git log for
fix/revert density, and `bbox_blame` for line-level provenance where relevant.

Correction-plan output: transcript anchors, dates, current code state, trend,
and how history corroborates or reorders other atom diagnoses.

## Triangulation examples

A useful Rust pathology result is usually a cluster, not a single atom firing.

Example god impl:

- Impl Role Coherence says `BlackboxServer` mixes HTTP/MCP routing, provider
  config, transcript stores, and orchestration helpers.
- State Ownership Collapse says the same impl's method clusters read distinct
  field sets that could be extracted into state/delegate structs.
- Test-Implied Rust Architecture says tests need broad `super::*` access to
  build only one behavior slice.

The correction plan should emit one diagnosis: `BlackboxServer` is both a
runtime owner and a domain-service bag. Ordered slices might be "extract store
state", "split provider config methods to a submodule", "introduce a trait seam
for tests", and "validate with cargo check/test".

Example provider dispatch:

- Impl Role Coherence says provider enum methods contain both provider specs
  and behavior.
- Module Topology Drift says one file owns model catalogs, CLI arg rendering,
  MCP env wiring, event parsing, and session discovery.
- Feature Matrix Entanglement says optional provider behavior is represented as
  scattered match arms rather than provider-family modules.

The plan should not emit three findings. It should emit one correction:
separate provider data specs from driver behavior, preserve shared driver
families, and use the raw `rust_match_arm_to_strategy` plan kind (RX-P1, no
wrapping atom) or PD slices where existing atoms do not cover the exact rewrite.

## Plan document shape

Rust correction plans use the same shape as Java architecture pathology, with
two Rust-specific additions: `authority_grades` and `atom_mapping`.

Path:

```text
<project>/design/refactor/plans/<slug>.md
```

Required frontmatter:

```yaml
---
title: "Rust Architecture Correction Plan: <scope>"
kind: correction-plan
lifecycle: proposed
corpus: <project>-refactor
topic:
  - refactor-plan
  - rust
  - architecture
date: <YYYY-MM-DD>
baseline_commit: <full-sha>
generated_by: rust-arch-pathology
scope: "<operator-readable scope>"
brief: "<one-line>"
---
```

Required body sections:

- `## Diagnosis Summary` - surviving Rust architecture diagnoses and why they
  matter.
- `## Evidence` - code refs, measurements, cargo/rust-analyzer authority,
  transcript anchors, and uncertainty.
- `## Authority Grades` - which claims are `syntax_only`, `indexed_hints`,
  `lsp_verified`, or compiler-confirmed; operator-supplied hints are recorded
  as provenance, not authority grades.
- `## Atom Mapping` - for each remediation slice: shipped atom by exact
  manifest name and any operator-authority flags it requires, gap ID such as
  G10-G23 when no atom is shipped, or `PD-manual` when no atom is planned.
  "Future atom" without an ID is not allowed.
- `## Remediation Plan` - ordered bounded slices PD can implement.
- `## Acceptance Criteria` - stable criteria with IDs such as `RAP-1`.
- `## Deferred` - rejected, speculative, compiler/lint-shaped, or future
  candidates.

Example acceptance criteria:

```json
[
  {
    "id": "RAP-1",
    "criterion_text": "The provider catalog data and provider driver behavior are no longer owned by the same impl/file; public API deltas are surfaced before apply."
  },
  {
    "id": "RAP-2",
    "criterion_text": "Each extracted Rust slice validates with cargo check and the targeted cargo test command named in the plan."
  },
  {
    "id": "RAP-3",
    "criterion_text": "The correction plan labels every remediation slice with a shipped atom, a G-series gap ID, or PD-manual execution."
  },
  {
    "id": "RAP-4",
    "criterion_text": "Any rust-analyzer-backed remediation step fails closed on lsp_unavailable rather than falling back to syntax-only rewrites."
  }
]
```

## Remediation handoff

Remediation uses the existing PD invocation shape documented in
[pd-dispatch.md](../../../docs/pd-dispatch.md). Pathology does not require PD
to learn Rust-specific fields.

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
      { "id": "RAP-1", "criterion_text": "..." }
    ]
  }
}
```

A future wrapper may read a reviewed correction plan and assemble this payload.
That wrapper is convenience around PD, not a new diagnosis artifact model.

## Existing atom mapping

The correction plan should use a diagnosis-to-execution matrix rather than a
flat atom catalog:

| Pathology atom | Shipped execution/evidence atoms | Gap-backed or manual needs |
|---|---|---|
| Impl Role Coherence | `rust-impl-partition-graph`, `rust-extract-impl-methods`, `rust-split-god-impl`, `rust-extract-to-submodule` | PD-manual when partitioning depends on judgment no atom can encode |
| State Ownership Collapse | `rust-state-extract`, sometimes sequenced with `rust-split-god-impl` | `acknowledge_repr` is operator-gated |
| Construction Boundary Collapse | `rust-split-god-impl` when the builder lives on an impl | PD-manual otherwise |
| Trait Boundary Mismatch | `rust-trait-from-impl` for missing trait surfaces | `rust-split-trait` G19; `rust_find_references` G10 for impact |
| Module Topology Drift | `rust-extract-to-submodule`, `rust-bin-to-lib-migration` | `rust_restructure_module_tree` G14; `rust_inline_module` G17; G22 macro caveat for bin-to-lib |
| Error Contract Drift | `rust-error-migrate`, `rust-public-api-guard` | `rust_wrap_return_in_result` G16; `acknowledge_public_api_change` is operator-gated |
| Feature / Cfg Matrix | `rust-cargo-add-dep` / raw `ensure_toml_table` for Cargo feature tables | `rust_add_cfg_attribute` G15; PD-manual cfg movement |
| Async / Runtime Lifecycle | `rust-state-extract` only for state relocation | PD-manual for cancellation, channels, task ownership |
| Test-Implied Architecture | `rust-test-island-extract`, `rust-trait-from-impl`, `rust-state-extract`, `rust-extract-impl-methods` | `rust_test_attribution` G18 |
| Unsafe Contract Opacity | `rust-public-api-guard` as supporting preflight | `rust_doc_harden` G23 may support invariant docs; no mutating safety atom |
| Macro-Generated Contract Opacity | `rust-public-api-guard` as supporting preflight | macro-expansion measurement; PD-manual |
| Transcript-Anchored Pressure | no mutating atom; evidence synthesis only | none |

Supporting atoms such as `rust-rename-symbol`, `rust-organize-imports`,
`rust-cargo-add-dep`, and `rust-public-api-guard` may appear inside remediation
slices, but they are not usually pathology diagnoses.

## Rejected or deferred from v0

- **Raw clippy findings.** Inputs only. If clippy can name it, pathology should
  not spend LLM time renaming it.
- **Dead code and unused imports.** rustc/rust-analyzer/clippy own these.
- **Method count / file size by itself.** Hotspot signal, not diagnosis.
- **"Should use async_trait" or crate-style preferences.** Local idiom, not
  architecture, unless tied to a trait boundary or runtime lifecycle diagnosis.
- **Generic newtype advocacy.** Useful when paired with state ownership or API
  boundary drift; not a standalone v0 pathology.
- **Unsafe block inventory.** Inventory is mechanical. Pathology owns the
  hidden-contract diagnosis only when callers depend on the unsafe invariant.
- **Macro expansion claims without expansion evidence.** Record uncertainty or
  request a measurement pass.
- **Feature flag scatter by count alone.** Needs a domain-boundary claim and
  a validation matrix.
- **Question-mark conversion claims without compiler validation.** Whether a
  `?` site converts under a new error type is rustc territory. Pathology
  records the contract change; compiler-backed remediation audits conversion
  gaps.
- **Constructor or builder size by itself.** Large constructors are hotspot
  signals, not diagnoses.
- **`Arc<Mutex<T>>` / `Rc<RefCell<T>>` proliferation by itself.** Runtime
  borrow workarounds may indicate missing architecture, but they are not
  diagnoses without a lifecycle or ownership claim.
- **Serde boundary bleed by itself.** `#[derive(Serialize, Deserialize)]` is not
  pathology. It becomes v0-relevant when serde attributes drive architectural
  boundaries and pair with state ownership collapse or module topology drift.
- **External crate dependency audits.** `cargo-deny`, `cargo-audit`, and
  `cargo-vet` own supply-chain concerns. Pathology owns only the case where a
  dependency choice defines an architectural seam without an explicit boundary.

## Risks and design choices

**Compiler collapse.** The largest Rust-specific risk is treating compiler
diagnostics as architecture findings. The compiler is evidence and validation;
pathology must still make an ownership claim.

**Overclaiming indexed hints.** Tree-sitter and local indexes are hints. Plans
must label them as such and use rust-analyzer or cargo validation when the
remediation depends on binding, trait, borrow, or import correctness.

**Macro blindness.** Macro-heavy code can invalidate apparent ownership. The
plan must say whether macro expansion was inspected and avoid claims that
depend on generated code when it was not.

**Operator authority.** Public API and `#[repr]` opt-outs remain explicit. A
pathology plan may request approval; it must not imply the execution atom can
grant it.

**Atom availability.** Pathology can recommend future atoms only when it labels
the substrate gap. Do not hide unavailable primitives inside a PD plan as if
they were shipped.

## Future work

- Implement a `rust-arch-pathology` workflow artifact with cheap survey,
  hotspot selection, focused atom runs, whiteboard review, and correction-plan
  emission.
- Add a Rust pathology brofile/persona that knows the SAST/compiler gate and
  authority-grade vocabulary.
- Add optional `cargo metadata` and cfg-density measurement helpers if current
  workspace tools make feature-matrix surveys too ad hoc.
- Add a wrapper that reads reviewed Rust correction plans and dispatches
  `phase-decompose-main-edit` with generated initial vars.
- Revisit deferred atoms after real pathology runs show whether their
  LLM-to-signal ratio is acceptable.

## Relationship to existing designs

- Sibling language pack for [Architecture Pathology](../arch-pathology.md).
- Upstream of [Rust Refactor Expansion](refactor-rust-expansion.md),
  [Rust Refactor Atoms - Batch 2](rust-refactor-atoms-batch2.md), and
  [Rust Refactor Gap Inventory](rust-refactor-gap-inventory.md).
- Uses the safety invariants from
  [Rust Refactor - v2 invariants and deferred plan kinds](refactor-rust-v2-invariants.md).
- Related to [Performance Pathology](../perf-pathology.md): same
  diagnosis-plan handoff shape, different evidence standards.

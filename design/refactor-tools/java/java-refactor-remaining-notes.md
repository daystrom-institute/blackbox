---
title: "Java Refactor Remaining Notes"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - java
tags:
  - refactor-tools
  - java
  - gap-notes
status: "implemented in worktree; refreshed 2026-05-19"
brief: "Grounded inventory of Java refactor notes after checking current plan-kind code, atom manifests, merge commits, and the worktree-java-refactor-remaining implementation pass."
---

# Java Refactor Remaining Notes

This document supersedes ad hoc Java refactor note triage after the original
`java-refactor-gaps.md` archive. It is grounded in current code, not just bbox
note resolution state: several notes still appear unresolved in the note store
even though the corresponding code landed.

Grounding references:

- Plan-kind dispatch table: `src/refactor/mod.rs`
- Java module exports: `src/refactor/java.rs`
- Java refactor implementation modules: `src/refactor/java/*.rs`
- Atom coverage guard: `src/orchestration/atoms/validate.rs`
- Java runbook: `system-defaults/memories/refactor-java.md`
- Merge commits: `db218899bb69e2273a07b550f88677d93f34dbce` and
  `ea553c668689a69311cf2c550902c0ebb170d432`

## Current Code Surface

`bbox_refactor_plan` currently dispatches the Java gap-family plan kinds below:

- `prune_java_orphans`
- `extract_java_code_block_to_method`
- `convert_method_to_class`
- `inline_java_class`
- `inline_java_method`
- `extract_java_test_slice`
- `java_collapse_call_chain`
- `migrate_java_method_receiver`
- `java_split_provider`
- `replace_java_static_reference`
- `singletonify_java_holder`
- `singletonify_java_util`
- `java_class_dependency_analysis`
- `extract_java_class_cohesive_clusters`
- `cluster_inject_params_java`
- `java_concurrency_antipattern_audit`
- `java_vaadin_provider_binding_generation`
- `java_lsp_organize_imports`
- `lombokify_java_class`

The atom coverage guard includes the shipped Java atoms for these plan kinds.
The remaining work is therefore not "basic Java plan-kind coverage"; it is
residual higher-order workflow and optional project-setup helper work.

## Notes Already Landed In Code

These notes should be treated as implementation-complete unless a fresh defect
is found:

| Note | Status | Grounding |
|---|---|---|
| `note-6020580c` Safe Dead Code / Orphan Elimination | Landed | `prune_java_orphans`, `java-prune-orphans` atom |
| `note-188c6fc9` Intra-Method Extraction | Landed | `extract_java_code_block_to_method`; v2 capture inference landed in `ea553c6` |
| `note-bd2b7a24` Replace Method with Method Object | Landed | `convert_method_to_class`; enclosing-field capture landed in `ea553c6` |
| `note-ea483190` Automated Test Slicing and Mocking | Landed | `extract_java_test_slice`; Mockito mixed-test synthesis landed in `e84eda2` / `ea553c6` |
| `note-8d4674ad` Inline Java Primitives | Landed in worktree | `inline_java_method` plus `inline_java_class` |
| `note-295e99e1` `java_collapse_call_chain` | Landed | N-segment and project-wide support landed in `286f51e` / `ea553c6` |
| `note-1ee49c59` `migrate_java_method_receiver` | Landed | auto-injection and project-wide caller walk exist |
| `note-4ec8ff30` `java_split_provider` | Landed | call-site rewrite, provider auto-injection, and full-coverage old-field deletion exist |
| `note-7d4f0001` Vaadin static lookup caller rewrite | Landed for callers | `replace_java_static_reference` drop-accessor mode and `java-replace-vaadin-static-lookup` atom exist |
| Vaadin Flow view decomposition/synthesis toolsuite | Landed | `java_vaadin_view_structure_analysis`, component/grid/dialog extraction, static UI audit, route inventory, view synthesis, route access, navigation helper extraction, and Vaadin workflow atoms |
| `note-7c819189` static holder caller rewrite | Landed for callers | `replace_java_static_reference` field mode and `java-migrate-static-holder` atom exist |
| `note-e5439c0a` static util caller rewrite | Landed for callers | `replace_java_static_reference` method mode and `java-singletonify-static-util` atom exist |
| `note-4257caaa` production-side holder/util conversion | Landed in worktree | `singletonify_java_holder`, `singletonify_java_util`, and Guice-only `java_vaadin_provider_binding_generation` exist |
| `note-e09391c4` shared DI plumbing helper | Landed | `src/refactor/java/di_plumbing.rs` plus use in receiver, provider, and static-reference rewrites |
| `note-0c749e44` Mockito stub synthesis | Landed | `extract_java_test_slice` mixed-test Mockito mode |
| `note-8eaf7bac` organize-imports newline / cold classpath weakness | Landed enough for original gap | JDTLS readiness drain and fallback hardening landed in `db21889`; any new JDTLS behavior should be filed separately |
| `note-b4df960d`, `note-0d97bb62`, `note-6353e60b`, `note-38ee1546` Lombok hardening | Addressed | implemented in `fb5169b` |

Note-store cleanup for the stale landed notes was done on 2026-05-19 with
references to the grounding commits. Future fresh defects in these surfaces
should get new, narrower notes instead of reopening this closed tranche.

## Worktree Coverage

The `worktree-java-refactor-remaining` branch covers the primitive/tooling items,
the optional Vaadin project-setup helper, the higher-order workflow atoms, and
the prompt/runbook drift:

- `inline_java_class` plus `java-inline-class` atom and eval coverage.
- `extract_java_class_cohesive_clusters` plus
  `java-extract-class-cohesive-clusters` atom and eval coverage.
- `cluster_inject_params_java` plus `java-cluster-inject-params` atom and eval
  coverage.
- `java_concurrency_antipattern_audit` plus
  `java-concurrency-antipattern-audit` atom and eval coverage.
- `java_vaadin_provider_binding_generation` plus
  `java-vaadin-provider-binding-generation` atom and eval coverage.
- Higher-order workflow atoms and eval coverage:
  `java-decompose-god-class`, `java-introduce-repository-pattern`,
  `java-split-god-method`, and `java-eliminate-dead-code`.
- Auto-injection prompt/runbook updates for `java-migrate-static-holder`,
  `java-singletonify-static-util`, `java-replace-vaadin-static-lookup`, and the
  Java refactor runbook.

## Remaining / Tracked Work

### 1. `inline_java_class`

Source note: residual part of `note-b433e29e`.

`inline_java_method` now supports multi-statement void bodies and non-private
project-wide inlining. The worktree adds the missing sibling primitive:
`inline_java_class`. Given a class instantiated in exactly one place, it inlines
the class into the caller by hoisting constructor-assigned fields to locals and
splicing the primary behavior method body into the call site.

Acceptance criteria:

- Refuse when the class has more than one construction site.
- Refuse or report when inlined members depend on private members no longer
  visible from the caller.
- Preserve constructor argument evaluation order.
- Emit imports required by the inlined body or refuse with structured leftovers.
- Ship an atom wrapper and coverage in the atom eval catalogs.

Status in worktree: implemented as `inline_java_class` with a conservative
`java-inline-class` atom. The v1 planner refuses unsupported imports/member
dependencies rather than attempting semantic import repair.

### 2. Cohesive Class Cluster Suggestions

Source note: `note-8967d541`.

`java_class_dependency_analysis` exposes the raw class graph, including methods,
fields, inner types, annotations, and edges. The worktree adds
`extract_java_class_cohesive_clusters`, which uses the field-touch and
method-call graph to propose `extract_java_class` partitions for a god class.

Acceptance criteria:

- Compute method-to-field and method-to-method affinity for the selected class.
- Suggest named clusters with item_names, move_fields, cross-cluster calls, and
  expected delegate/callback wiring.
- Bias names from method prefixes and package/domain vocabulary, but keep the
  operator in the loop.
- Emit analysis only in v1; do not write files.
- Round-trip each accepted cluster into an `extract_java_class` plan.

Status in worktree: analysis-only implemented with
`java-extract-class-cohesive-clusters`. Accepted clusters still round-trip via a
separate operator-run `java-extract-cohesive-class` atom.

### 3. Constructor Parameter Clustering

Source note: `note-a9f12f09`.

`java_class_dependency_analysis` reports class methods, fields, inner types,
annotations, and edge data. The worktree adds `cluster_inject_params_java` to
analyze an oversized `@Inject` constructor, derive co-use groups from method
bodies, and propose parameter-object extraction.

Acceptance criteria:

- Build a parameter-to-method usage graph for a selected constructor.
- Suggest cohesive parameter-object groups with scores and naming hints.
- Report methods spanning multiple groups before writing anything.
- v1 should be analysis-only and emit proposed follow-up plans rather than
  mutating source.
- v2 may generate holder classes and caller rewrites.

Status in worktree: analysis-only implemented with `java-cluster-inject-params`.
Holder class generation and caller rewrites remain future v2 work.

### 4. Java Concurrency Antipattern Audit

Source note: `note-bb036628`.

The worktree adds `java_concurrency_antipattern_audit` for
`Collections.synchronizedMap(...)` combined with `computeIfAbsent`, `merge`, or
`compute`, and the analogous `synchronizedSet` + `removeIf` pattern. This is
tooling/audit work, not a mechanical refactor primitive.

Acceptance criteria:

- Detect synchronized collection declarations and trace variable uses in the
  same file first.
- Flag unsafe compound operations outside an explicit `synchronized(collection)`
  block.
- Report file, line, variable, collection wrapper, operation, and confidence.
- Prefer a `bbox_lint` / audit surface or a read-only refactor analysis plan;
  do not auto-rewrite to concurrent collections.

Status in worktree: read-only refactor analysis plan implemented with
`java-concurrency-antipattern-audit`; it does not rewrite collections.

### 5. Vaadin Provider Binding Generation

Source notes: residual from `note-4257caaa` / `note-7d4f0001`.

Caller-side static lookup rewrites are shipped, and production-side
`singletonify_java_holder` / `singletonify_java_util` are shipped. The remaining
unmechanized part is generating or verifying the project-level binding that
provides `Provider<UI>` and `Provider<VaadinSession>` in Vaadin + Guice /
Jakarta projects.

The `ea553c6` merge explicitly classified this as operator workflow rather than
recurring refactor machinery. This worktree promotes it into the intended shape:
a small project-setup helper with framework detection, not part of every caller
rewrite.

Acceptance criteria if implemented:

- Detect Vaadin + Guice/Jakarta integration style.
- Locate or create the appropriate module/config class.
- Add provider methods only when absent.
- Refuse Spring/Vaadin variants unless explicitly supported.

Status in worktree: implemented as the Guice-only
`java_vaadin_provider_binding_generation` helper with
`java-vaadin-provider-binding-generation` atom and eval coverage. v1 requires
the operator to pass the intended Guice module/config source file; it verifies
Vaadin presence, refuses Spring/Vaadin variants, adds missing imports/provider
methods only when absent, and treats project builds as operator-run outside the
atom.

### 6. Higher-Order Java Refactor Atoms

Source notes: `note-de327325`, `note-0cb730ac`, `note-39a34eb6`,
`note-66e25c58`.

The primitive pieces exist for parts of these workflows, but the orchestrator
atoms do not:

- `java-decompose-god-class`: use dependency/class analysis to propose seams,
  then iterate `java-extract-cohesive-class` and test slicing.
- `java-introduce-repository-pattern`: find raw DB access in a package and move
  it into repository classes.
- `java-split-god-method`: compose `convert_method_to_class` and repeated
  `extract_java_code_block_to_method`.
- `java-eliminate-dead-code`: loop `prune_java_orphans` and
  `java_lsp_organize_imports` until convergence.

These should be workflow-backed atoms or supervised workflows, not new primitive
plan kinds, because their value is sequencing, review gates, and validation
loops over already-shipped primitives.

Status in worktree: implemented as workflow-backed atoms with discovery,
dispatch, and behavior-smoke eval coverage:

- `java-decompose-god-class`
- `java-introduce-repository-pattern`
- `java-split-god-method`
- `java-eliminate-dead-code`

These remain supervised atom workflows over existing primitives rather than new
primitive plan kinds. Their prompts use operator approval gates and avoid
Maven/Gradle execution inside `bbox_refactor_run`, matching the atom-dispatch
command allowlist.

### 7. Prompt And Runbook Drift

Some shipped atom prompts still said the operator must pre-stage provider fields
before static-reference or provider-split rewrites. Current code can synthesize
`@Inject` fields via `delegate_type` / `getter_types` and the shared
`di_plumbing` helper; the worktree updates the prompt/runbook wording to expose
that mode.

Follow-up:

- Update `java-migrate-static-holder`, `java-singletonify-static-util`, and
  `java-replace-vaadin-static-lookup` prompts to expose auto-injection mode
  where the plan kind supports it.
- Update `sm-refactor-java` wording that still says v1 requires manual
  provider field staging for split-provider / static-reference cases.
- Keep manual mode documented as an operator override.

Status in worktree: addressed.

## Non-Goals

- Do not reopen the archived `java-refactor-gaps.md` as a live backlog.
- Do not file new gap notes for capabilities already landed but unresolved in
  the note store; resolve the existing notes instead.
- Do not treat Java architecture-pathology smoke followups as Java refactor
  primitive gaps unless they require a new reusable refactor or analysis
  surface.

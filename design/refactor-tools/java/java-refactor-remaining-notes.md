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
status: "grounded 2026-05-19"
brief: "Grounded inventory of Java refactor notes after checking current plan-kind code, atom manifests, and merge commits."
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
- `inline_java_method`
- `extract_java_test_slice`
- `java_collapse_call_chain`
- `migrate_java_method_receiver`
- `java_split_provider`
- `replace_java_static_reference`
- `singletonify_java_holder`
- `singletonify_java_util`
- `java_class_dependency_analysis`
- `java_lsp_organize_imports`
- `lombokify_java_class`

The atom coverage guard includes the shipped Java atoms for these plan kinds.
The remaining work is therefore not "basic Java plan-kind coverage"; it is
residual higher-order workflow, analysis, or missing sibling primitives.

## Notes Already Landed In Code

These notes should be treated as implementation-complete unless a fresh defect
is found:

| Note | Status | Grounding |
|---|---|---|
| `note-6020580c` Safe Dead Code / Orphan Elimination | Landed | `prune_java_orphans`, `java-prune-orphans` atom |
| `note-188c6fc9` Intra-Method Extraction | Landed | `extract_java_code_block_to_method`; v2 capture inference landed in `ea553c6` |
| `note-bd2b7a24` Replace Method with Method Object | Landed | `convert_method_to_class`; enclosing-field capture landed in `ea553c6` |
| `note-ea483190` Automated Test Slicing and Mocking | Landed | `extract_java_test_slice`; Mockito mixed-test synthesis landed in `e84eda2` / `ea553c6` |
| `note-8d4674ad` Inline Java Primitives | Partially landed | `inline_java_method` exists; see remaining `inline_java_class` below |
| `note-295e99e1` `java_collapse_call_chain` | Landed | N-segment and project-wide support landed in `286f51e` / `ea553c6` |
| `note-1ee49c59` `migrate_java_method_receiver` | Landed | auto-injection and project-wide caller walk exist |
| `note-4ec8ff30` `java_split_provider` | Landed | call-site rewrite, provider auto-injection, and full-coverage old-field deletion exist |
| `note-7d4f0001` Vaadin static lookup caller rewrite | Landed for callers | `replace_java_static_reference` drop-accessor mode and `java-replace-vaadin-static-lookup` atom exist |
| `note-7c819189` static holder caller rewrite | Landed for callers | `replace_java_static_reference` field mode and `java-migrate-static-holder` atom exist |
| `note-e5439c0a` static util caller rewrite | Landed for callers | `replace_java_static_reference` method mode and `java-singletonify-static-util` atom exist |
| `note-4257caaa` production-side holder/util conversion | Partially landed | `singletonify_java_holder` and `singletonify_java_util` exist; Vaadin provider binding generation remains operator workflow |
| `note-e09391c4` shared DI plumbing helper | Landed | `src/refactor/java/di_plumbing.rs` plus use in receiver, provider, and static-reference rewrites |
| `note-0c749e44` Mockito stub synthesis | Landed | `extract_java_test_slice` mixed-test Mockito mode |
| `note-8eaf7bac` organize-imports newline / cold classpath weakness | Landed enough for original gap | JDTLS readiness drain and fallback hardening landed in `db21889`; any new JDTLS behavior should be filed separately |
| `note-b4df960d`, `note-0d97bb62`, `note-6353e60b`, `note-38ee1546` Lombok hardening | Addressed | implemented in `fb5169b` |

Note-store cleanup for the stale landed notes was done on 2026-05-19 with
references to the grounding commits. Future fresh defects in these surfaces
should get new, narrower notes instead of reopening this closed tranche.

## Remaining Implementation Work

### 1. `inline_java_class`

Source note: residual part of `note-b433e29e`.

`inline_java_method` now supports multi-statement void bodies and non-private
project-wide inlining. The missing sibling primitive is still `inline_java_class`:
given a class instantiated in exactly one place, inline the class into the
caller by hoisting constructor arguments / fields to locals and splicing the
primary behavior method body into the call site.

Acceptance criteria:

- Refuse when the class has more than one construction site.
- Refuse or report when inlined members depend on private members no longer
  visible from the caller.
- Preserve constructor argument evaluation order.
- Emit imports required by the inlined body or refuse with structured leftovers.
- Ship an atom wrapper and coverage in the atom eval catalogs.

### 2. Cohesive Class Cluster Suggestions

Source note: `note-8967d541`.

`java_class_dependency_analysis` now exposes the raw class graph, including
methods, fields, inner types, annotations, and edges. It does not yet turn that
graph into suggested extraction clusters. The open capability is
`extract_java_class_cohesive_clusters`: use the field-touch and method-call graph
to propose `extract_java_class` partitions for a god class.

Acceptance criteria:

- Compute method-to-field and method-to-method affinity for the selected class.
- Suggest named clusters with item_names, move_fields, cross-cluster calls, and
  expected delegate/callback wiring.
- Bias names from method prefixes and package/domain vocabulary, but keep the
  operator in the loop.
- Emit analysis only in v1; do not write files.
- Round-trip each accepted cluster into an `extract_java_class` plan.

### 3. Constructor Parameter Clustering

Source note: `note-a9f12f09`.

`java_class_dependency_analysis` now reports class methods, fields, inner types,
annotations, and edge data, but it is not a constructor-parameter partitioner.
The open capability is `cluster_inject_params_java`: analyze an oversized
`@Inject` constructor, derive co-use groups from method bodies, and propose
parameter-object extraction.

Acceptance criteria:

- Build a parameter-to-method usage graph for a selected constructor.
- Suggest cohesive parameter-object groups with scores and naming hints.
- Report methods spanning multiple groups before writing anything.
- v1 should be analysis-only and emit proposed follow-up plans rather than
  mutating source.
- v2 may generate holder classes and caller rewrites.

### 4. Java Concurrency Antipattern Audit

Source note: `note-bb036628`.

No Java-specific concurrency audit exists for `Collections.synchronizedMap(...)`
combined with `computeIfAbsent`, `merge`, or `compute`, nor for the analogous
`synchronizedSet` + `removeIf` pattern. This is tooling/audit work, not a
mechanical refactor primitive.

Acceptance criteria:

- Detect synchronized collection declarations and trace variable uses in the
  same file first.
- Flag unsafe compound operations outside an explicit `synchronized(collection)`
  block.
- Report file, line, variable, collection wrapper, operation, and confidence.
- Prefer a `bbox_lint` / audit surface or a read-only refactor analysis plan;
  do not auto-rewrite to concurrent collections.

### 5. Vaadin Provider Binding Generation

Source notes: residual from `note-4257caaa` / `note-7d4f0001`.

Caller-side static lookup rewrites are shipped, and production-side
`singletonify_java_holder` / `singletonify_java_util` are shipped. The remaining
unmechanized part is generating or verifying the project-level binding that
provides `Provider<UI>` and `Provider<VaadinSession>` in Vaadin + Guice /
Jakarta projects.

The `ea553c6` merge explicitly classifies this as operator workflow rather than
recurring refactor machinery. If it is promoted into tooling, it should be a
small project-setup helper with framework detection, not part of every caller
rewrite.

Acceptance criteria if implemented:

- Detect Vaadin + Guice/Jakarta integration style.
- Locate or create the appropriate module/config class.
- Add provider methods only when absent.
- Refuse Spring/Vaadin variants unless explicitly supported.

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

### 7. Prompt And Runbook Drift

Some shipped atom prompts still say the operator must pre-stage provider fields
before static-reference or provider-split rewrites. Current code can synthesize
`@Inject` fields via `delegate_type` / `getter_types` and the shared
`di_plumbing` helper.

Follow-up:

- Update `java-migrate-static-holder`, `java-singletonify-static-util`, and
  `java-replace-vaadin-static-lookup` prompts to expose auto-injection mode
  where the plan kind supports it.
- Update `sm-refactor-java` wording that still says v1 requires manual
  provider field staging for split-provider / static-reference cases.
- Keep manual mode documented as an operator override.

## Non-Goals

- Do not reopen the archived `java-refactor-gaps.md` as a live backlog.
- Do not file new gap notes for capabilities already landed but unresolved in
  the note store; resolve the existing notes instead.
- Do not treat Java architecture-pathology smoke followups as Java refactor
  primitive gaps unless they require a new reusable refactor or analysis
  surface.

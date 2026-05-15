# Java Refactor Tooling: Shipped, Gapped, and Addressed

A catalog of Java refactor plan kinds, atoms, inline gap markers, and
cross-language coverage. Generated 2026-05-15.

## Shipped: 20 Java Refactor Plan Kinds

Dispatched via `bbox_refactor_plan(kind="...")`. Dispatch table verified in
`src/refactor/mod.rs:1171-1190`.

### Extraction and Move (5)

| Kind | Dispatch Source |
|------|----------------|
| `extract_java_class` | `plan_extract_java_class` — composite: methods + field moves + delegate wiring + caller rewrite |
| `extract_java_methods` | `plan_extract_java_methods` — extract methods to new or existing class |
| `extract_java_nested_classes` | `plan_extract_java_nested_classes` — lift nested/inner classes to top-level |
| `extract_java_interface` | `plan_extract_java_interface` — extract interface from concrete class |
| `promote_java_inner_class` | `plan_promote_java_inner_class` — promote inner class to top-level file |

### Leaf Primitives (11)

| Kind | Dispatch Source |
|------|----------------|
| `add_java_fields` | `plan_add_java_fields` — add field declarations |
| `add_java_constructor` | `plan_add_java_constructor` — generate constructor with parameter wiring |
| `add_java_delegate_field` | `plan_add_java_delegate_field` — add delegate field + constructor wiring |
| `add_java_implements` | `plan_add_java_implements` — add `implements` clause |
| `move_java_field` | `plan_move_java_field` — move field to another class |
| `move_java_constant` | `plan_move_java_constant` — move static constant |
| `update_java_callers` | `plan_update_java_callers` — rewrite call sites after signature change |
| `rewrite_java_visibility` | `plan_rewrite_java_visibility` — change visibility of methods/fields/classes |
| `rename_java_symbol` | `plan_rename_java_symbol` — rename symbol (tree-sitter backed) |
| `migrate_java_type_usages` | `plan_migrate_java_type_usages` — rewrite type references across files |
| `lombokify_java_class` | `plan_lombokify_java_class` — replace hand-rolled boilerplate with Lombok annotations |

### Analysis-Only (4)

| Kind | Dispatch Source |
|------|----------------|
| `find_java_usages` | `plan_find_java_usages` — enumerate call sites with declaring_class filter |
| `java_class_dependency_analysis` | `plan_java_class_dependency_analysis` — class dependency report |
| `java_public_api_guard` | `plan_java_public_api_guard` — surface public API delta severity |
| `java_lsp_organize_imports` | `plan_java_lsp_organize_imports` — JDTLS-backed import organization |

Note: `java_lsp_organize_imports` has a heuristic fallback when JDTLS is
unavailable (`leaf_plans.rs:544` logs and falls back to heuristic), unlike the
Rust LSP plan kinds which fail closed per RX-V3.

## Shipped: 6 Java Refactor Atoms

Verified via `bro_agent_list` (2026-05-15). Active (non-superseded) entries only.

| Atom | Version | Cost Class | Plan Kind(s) |
|------|---------|------------|--------------|
| `java-extract-cohesive-class` | v3 | normal | `extract_java_class` |
| `java-extract-interface` | v2 | normal | `extract_java_interface`, `java_public_api_guard` (v2 preflight) |
| `java-lombokify` | v1 | expensive | `lombokify_java_class` |
| `java-promote-inner-class` | v1 | normal | `promote_java_inner_class` |
| `java-class-dependency-graph` | v2 | cheap | `java_class_dependency_analysis` |
| `java-public-api-guard` | v1 | normal | `java_public_api_guard` |

Superseded versions: `java-extract-cohesive-class` v1-v2, `java-extract-interface`
v1, `java-class-dependency-graph` v1.

## Inline Gap Markers: 27 Distinct "Gap N" Comments

`Gap N` markers appear as inline comments in `src/refactor/java/*.rs`. Missing
numbers 9 and 15 — 27 distinct numbered gaps across the range 1–29.

**Note**: The token `JAVA_TOOL_GAPS` does not exist in the codebase (0 grep
matches). The gap marker convention is bare `Gap N` comments.

### Gap 1 (lombokify.rs, extract_class.rs, tests.rs)
Primitive `boolean` field with hand-rolled `getFoo()` vs Lombok `isFoo()`.
Also: caller-rewrite zero-width inserts, method-call qualifier on LHS-write.

### Gap 2 (extract_class.rs)
No `optional` flag — `extract_java_class` fails the whole batch, can't skip a
file that "has no boilerplate." Also: target package derivation via unified
resolver.

### Gap 3 (tests.rs)
Method-reference qualifier is an instance field of the enclosing class;
captured_variables must surface this.

### Gap 4 (cross_file.rs, extract_class.rs, extract_methods.rs, imports.rs, tests.rs)
Cross-file static caller rewrite for `extract_java_class`. Moved static
methods/constants in other files get their call sites rewritten. Instance
methods are NOT rewritten cross-file. Also: type names as receivers of static
method calls.

### Gap 5 (extract_class.rs, tests.rs)
Source-class inner type references. When the target type lands in a different
package than the source, inner-type references need full qualification. Also:
source delegate wrappers for moved public methods.

### Gap 6 (extract_class.rs, tests.rs)
`deep_analysis` defaults false. `rewrite_remaining_accessors` decoupled from
`deep_analysis` — source-side reads should be rewritten through the delegate
but were silently miscompiling without `rewrite_remaining_accessors`.

### Gap 7 (extract_class.rs, atom_plans.rs, tests.rs)
When a captured-param name refers to a field rather than a constructor
parameter, wiring must follow the captured-field assignment. Also: inner-class
qualification for field-name conflicts.

### Gap 8 (extract_class.rs, tests.rs)
Stacked-extract topo ordering — wiring against accessor edits from prior
extracts. Also: surface ordering conflict when an accessor rewrite backtracks
past its field-only-capture lower bound.

### Gap 10 (atom_plans.rs, tests.rs)
Package + import derivation for extraction target. Class modifier rewriting
(`final` → non-`final`). Previous validation and tree-sitter checks.

### Gap 11 (atom_plans.rs, tests.rs)
Cross-package extracts: sibling-inner / outer-method references need
qualification. Same-package extracts do NOT inject source import.

### Gap 12 (atom_plans.rs, tests.rs)
Qualify references to other source-class inner types in the moved body.
Same-package extracts qualify but don't add imports; cross-package need both.

### Gap 13 (atom_plans.rs, tests.rs)
Constructor visibility during promotion — when the class header gets widened,
constructors must be rewritten. Protected constructors left alone.

### Gap 14 (leaf_plans.rs, tests.rs)
`extract_java_interface` imports from the source class.

### Gap 16 (extract_methods.rs, imports.rs, tests.rs)
`extract_java_methods` to an existing target appends rather than overwriting.
Also: `Outer.Inner` references must keep qualified form. Inner class detection
for import resolution.

### Gap 17 (extract_methods.rs, tests.rs)
Instance-method cross-class move detection — surface advisory when a selected
method is an instance method moving to a different class.

### Gap 18 (rename_symbol.rs, extract_class.rs, public_api_guard.rs, class_dependency.rs, tests.rs)
Serialization format mismatch: PlanStatus enum serializes wrong case. Affects
rename_java_symbol plan JSON, remaining_source_accessors rewrite, and capital-P
serialization regression.

### Gap 19 (tests.rs)
Captured_variables must resolve identifiers against the enclosing scope.

### Gap 20 (extract_class.rs, tests.rs)
Split captures into static-final constants vs instance fields. Static-finals
route through a constants path.

### Gap 21 (tests.rs)
Captured_variables must surface mutability indicators.

### Gap 22 (extract_class.rs)
Scaffold unresolved deps in generated target text. Insert FIXME comment lines
above external_call sites.

### Gap 23 (extract_class.rs)
Pick interfaces to inject into the target's class declaration. Also: imports
and scaffolding for inherited deps.

### Gap 24 (extract_class.rs, tests.rs)
Decide the visibility floor for delegate-rewritten methods. Extracted-method
visibility widening on the target.

### Gap 25 (extract_class.rs, tests.rs)
Prune unused imports on the generated target. Annotation propagation from
source to target (partially addressed by `propagate_class_annotations`).

### Gap 26 (extract_class.rs, tests.rs)
When fields are moved AND deep_analysis is on, surface every remaining source
accessor. Superclass/interface propagation when extracted methods depend on
inherited members.

### Gap 27 (tests.rs)
When the moved field appears on BOTH sides of an assignment, the LHS-write
split must handle qualified and unqualified forms.

### Gap 28 (imports.rs, tests.rs)
Drop explicit single-type imports already covered by a wildcard. Does NOT
drop `import static ...` lines.

### Gap 29 (extract_class.rs, tests.rs)
Warn the operator about mutable captures that were promoted to final fields.
Non-final captured field promotes to constructor parameter.

### Files by Gap Density

| File | Gaps |
|------|------|
| `extract_class.rs` | 1, 2, 4, 5, 6, 7, 8, 18, 20, 22, 23, 24, 25, 26, 29 |
| `tests.rs` | 1, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21, 24, 26, 27, 28, 29 |
| `atom_plans.rs` | 7, 10, 11, 12, 13 |
| `extract_methods.rs` | 4, 16, 17 |
| `imports.rs` | 4, 16, 28 |
| `cross_file.rs` | 4 |
| `rename_symbol.rs` | 18 |
| `leaf_plans.rs` | 14 |
| `public_api_guard.rs` | 18 |
| `class_dependency.rs` | 18 |
| `lombokify.rs` | 1 |
| `find_usages.rs` | (none) |
| `move_and_callers.rs` | (none) |

## Gap Notes (from bbox Note Store)

Java refactor gap notes exist in the note store but were not independently
verified for this revision. A claude-opus review (task `f2114523`, 2026-05-15)
reported:

- **5 unresolved `refactor_primitive` gap notes**: covering extract code block
  to method, inline method/class, convert method to class, extract test slice,
  and prune orphans.
- **4 addressed gap notes**: Lombok hardening fixes resolved by commit
  `fb5169b` (all related to `lombokify_java_class` planning robustness, not
  initial feature shipping).
- **4 workflow-atom gaps**: `java-decompose-god-class`,
  `java-introduce-repository-pattern`, `java-split-god-method`,
  `java-eliminate-dead-code` — filed as followup notes for higher-level
  refactoring workflows not yet atom-wrapped.
- **1 semantic-overlap gap**: `promote_java_inner_class` vs
  `extract_java_nested_classes` need clearer documentation of when to use each.

**Note**: The note IDs in the initial draft of this document were fabricated
and have been removed. Consult `bbox_notes(kind="followup", query="java")` for
current IDs.

## Atom-to-Plan-Kind Coverage

14 plan kinds lack atom wrappers. The 6 existing atoms cover extraction,
interface extraction, Lombok conversion, inner class promotion, dependency
analysis, and public API guarding. Missing atom coverage:

| Plan Kind | Notes |
|-----------|-------|
| `extract_java_methods` | No atom; composable primitive |
| `extract_java_nested_classes` | No atom; could be standalone |
| `add_java_fields` | No atom; low-level primitive |
| `add_java_constructor` | No atom; low-level primitive |
| `add_java_delegate_field` | No atom; low-level primitive |
| `add_java_implements` | No atom; low-level primitive |
| `move_java_field` | No atom; single-field move |
| `move_java_constant` | No atom; single-constant move |
| `update_java_callers` | No atom; composable primitive |
| `rewrite_java_visibility` | No atom; low-level primitive |
| `rename_java_symbol` | No atom; single-symbol rename |
| `migrate_java_type_usages` | No atom; cross-cutting, high blast radius |
| `find_java_usages` | No atom; analysis-only |
| `java_lsp_organize_imports` | No atom; single-step, would make a natural cheap atom |

The most obvious missing atoms are `java-lsp-organize-imports` (cheap,
single-step, no blast radius) and `java-extract-methods` (medium complexity,
clear use case).

## Cross-Language Gaps

### Java → Rust

Plan kinds that exist for Java but have no Rust equivalent:

- **`extract_java_interface`**: No `extract_rust_trait` existed at time of
  writing. However, `extract_rust_trait` appears in mod.rs:1196, so this may
  have shipped. Verify.
- **`lombokify_java_class`**: No Rust analog for boilerplate reduction via
  annotations. Rust uses `#[derive]` but has no "replace hand-rolled trait
  impls with derive macros" refactor.

### Rust → Java

Rust plan kinds with no Java equivalent:

- **`move_rust_struct_fields`**: No Java analog for moving multiple fields
  between structs (`move_java_field` is single-field).
- **`rewrite_rust_error_type`**: No Java analog for systematic error type
  rewriting (Java checked exceptions use different patterns).

### Shared Gaps

- **Extract code block to method/function**: Missing in both languages.
- **Inline method/function**: Missing in both languages.
- **Test slice extraction**: Missing in both.

## Review History

- **Initial draft** (2026-05-15): Contained fabricated note IDs, wrong atom
  counts/versions/cost-classes, incorrect Gap N count, and a reversed LSP
  fallback claim. All identified by claude-opus review (task `f2114523`).
- **This revision** (2026-05-15): Corrected against verified code state,
  `bro_agent_list` output, and `src/refactor/mod.rs` dispatch table. Gap note
  IDs remain unverified pending note store access.

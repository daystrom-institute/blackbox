# Java Refactor Plan-Kind Gaps

Audit performed: 2026-05-11.
Companion to: `design/proposed/refactor-agents.md`, this branch's
RA-A1..A7 Rust atom catalog landing.

Two gaps block the Java atom catalog from reaching the same shape as the
Rust catalog. Both are missing **plan kinds**, not missing atoms — an
atom layered on nothing isn't atomic.

## Current Java plan-kind surface (16, all routed via `src/refactor/mod.rs:978-993`)

| Plan kind | Purpose |
|---|---|
| `extract_java_methods` | Move method declarations into a new or existing class |
| `extract_java_class` | Composite: methods + field moves + delegate + caller delegation + accessor rewrites + cross-package widening + FIXMEs + callback_externals + inner-type qualification + overload disambiguation + default-interface satisfaction |
| `extract_java_nested_classes` | Syntactic nested-class extraction (static inners) |
| `promote_java_inner_class` | Non-static inner class → top-level class with capture-promoted ctor params |
| `add_java_fields` | Add `field_declaration` nodes to a class |
| `add_java_constructor` | Add a constructor with operator-supplied parameter list |
| `move_java_field` | Move instance field with delegate rewrite + accessor generation |
| `move_java_constant` | Move `static final` fields, preserving initializer |
| `update_java_callers` | Rewrite source-class call sites to route through a delegate field; covers method references |
| `add_java_delegate_field` | Inject `private final <T> <delegate>;` + first-constructor wiring |
| `rewrite_java_visibility` | Change visibility per method |
| `java_lsp_organize_imports` | JDTLS-backed organize-imports with structural fallback |
| `add_java_implements` | Add `implements <I>` to a class declaration |
| `extract_java_interface` | Extract interface from a class + add implements + widen visibility |
| `migrate_java_type_usages` | Replace type-use positions (concretion → interface) at variable/parameter/return/field type sites |
| `lombokify_java_class` | Hand-rolled POJO boilerplate → Lombok annotations; single-file and bulk-dir modes |

The Java surface is materially more capable than the current Rust
surface. `extract_java_class` alone subsumes the work split across
`extract_rust_impl_methods` + `move_rust_struct_fields` +
`add_rust_delegate_field` + `update_rust_callers` +
`rewrite_rust_item_visibility` on the Rust side. The Java atom catalog
is correspondingly thinner — 4 atoms cover what 7 Rust atoms cover —
*except for these two analysis-shaped gaps.*

---

## Gap 1 — `java_public_api_guard`

**Status:** LANDED 2026-05-11 on `java-tool-gaps-tranche-5`. Closing
commit ships the plan kind, dispatcher wiring, and 7 tests covering
the full severity matrix (breaking / caution / info), the mixed-
worst-case rule, directory-scoped scan with build-dir skip, and the
plan_status snake_case serialization contract. See
`src/refactor/java/public_api_guard.rs`.

**Rust counterpart:** `rust_public_api_guard` (RX-G2),
`src/refactor/rust_public_api.rs`. Returns a `PublicApiReport` with
`public_items_touched`, `public_api_delta_summary`,
`crate_root_re_exports_affected`, and an `advisory_severity`
classification (`info` / `caution` / `breaking`) against a set of
`ProposedChangeRef { file, item_name, change_kind }` inputs.

**Why an atom can't ship without it:** the design's
`java-extract-interface` atom is the natural use site. Renaming
`ServiceImpl` → `Service` at type-use positions is the kind of change
that flips an internal class into a public-surface guarantee for
callers in other Maven/Gradle modules. The rust-error-migrate-shaped
preflight pattern ("guard runs as a separate plan, OUTSIDE the mutating
`bbox_refactor_run`, with `acknowledge_public_api_change` operator-
authority gating the run") needs the guard plan kind to even exist.
The Rust atom (`rust-public-api-guard`, RA-A2) is `cost_class: normal`
and useful both standalone (audit-shaped) and as a preflight from
`rust-error-migrate` (RA-A6) — the Java equivalent would be useful at
the same two surfaces (standalone audit + preflight from
`java-extract-interface`).

**Sketch of the plan-kind contract:**

```text
bbox_refactor_plan(
  kind="java_public_api_guard",
  project_dir="<root>",
  source="<file or dir>",                       // scopes the scan
  deep_analysis=true,
  toml_entries={ "proposed_changes": [
    { "file": "<file>", "item_name": "<class|method|field>", "change_kind": "modify|remove|add" }
  ]}
)
```

Response shape (advisory; analogue of `PublicApiReport`):

```json
{
  "kind": "java_public_api_guard",
  "advisory_severity": "info|caution|breaking",
  "public_items_touched": [
    { "kind": "class|method|field", "fqcn": "...", "modifiers": "public", "line": 42 }
  ],
  "public_api_delta_summary": {
    "added_public": 0, "removed_public": 1, "modified_signatures": 2
  },
  "module_boundaries_affected": [ "..." ]   // optional, Maven/Gradle module-aware
}
```

**Why "module_boundaries_affected" instead of Rust's
`crate_root_re_exports_affected`:** Java has no `pub use` re-export
shape, but it does have module-info.java boundaries (JPMS), Maven /
Gradle multi-module project boundaries, and `package-info.java`
restricted-export hints. A Java guard should at least recognize when a
touched public symbol crosses a module-info `exports` boundary; the
project-type-index walk that `extract_java_class` already does
provides the cross-module references for free.

**Severity heuristic (mirror of Rust):**

- `breaking` — a `public` or `protected` item is removed, renamed, or
  its signature changed; OR a touched item is `exports`-named in a
  `module-info.java`.
- `caution` — a `package-private` item is touched but project-type-
  index shows cross-package callers (suggesting de-facto public use).
- `info` — only `private` / `package-private` items touched, no
  cross-package callers.

**Out of scope for the gap closer:** full classpath resolution (the
heuristic mirrors the existing `extract_java_class` project-type-index
walk; non-project callers are not detected, same as Rust's guard).

**Atom that unblocks once shipped:** `java-public-api-guard`
(parallel to RA-A2), cost_class: normal, standalone + preflight roles.

---

## Gap 2 — `java_class_dependency_analysis`

**Status:** LANDED 2026-05-11 on `java-tool-gaps-tranche-5`. Closing
commit ships the plan kind, dispatcher wiring, and 5 tests covering
class metadata + class-level annotations + methods + fields + inner
types, named-class selection via `module_name`/`impl_name`, the
outer-only filter (inner-class methods correctly excluded), and the
plan_status snake_case serialization contract. The v1 omits the
explicit edge graph (method_to_method / method_to_field /
method_to_inherited) per the contract sketch — that v2 wraps the
existing `analyze_extracted_dependencies` per-method walker into a
whole-class stitcher. See `src/refactor/java/class_dependency.rs`.

**Rust counterpart:** `rust_impl_partition_analysis` (RX-G1),
`src/refactor/mod.rs::plan_rust_impl_partition_analysis`. Returns a
method/field/edges graph for a specified impl block — pure analysis,
no mutation, no apply path. Drives the
`rust-impl-partition-graph` atom (RA-A1), which is the
analysis-only preflight to `rust-split-god-impl`.

**Why the existing reports don't satisfy the gap:**
`extract_java_class` already populates `captured_variables`,
`external_calls`, `inherited_dependencies`, and
`remaining_source_accessors` under `deep_analysis: true`. But these
fire only as a side effect of an extraction plan — they require the
operator to commit to a target file, an `item_names` list, and a
candidate set of `move_fields`. A class-shaped *graph* of "which
methods call which methods, which fields are read by which methods,
which inner classes capture what" before the operator has decided on
partitions is exactly what the Rust graph atom returns and what is
missing on the Java side.

**Sketch of the plan-kind contract:**

```text
bbox_refactor_plan(
  kind="java_class_dependency_analysis",
  project_dir="<root>",
  source="<file>",
  module_name="<ClassName>",                // optional when file has one top-level class
  deep_analysis=true                         // required; the report is the entire output
)
```

Response shape (analysis-only; analogue of Rust's
`rust_impl_partition_analysis`):

```json
{
  "kind": "java_class_dependency_analysis",
  "class": { "name": "DashboardView", "package": "com.example" },
  "methods": [
    { "name": "getMeterGrid", "signature": "Grid<Meter> getMeterGrid()",
      "line_range": [120, 145], "visibility": "public" }
  ],
  "fields": [
    { "name": "meterGrid", "type": "Grid<Meter>", "visibility": "private", "final": false }
  ],
  "inner_types": [
    { "name": "MeterRow", "kind": "static_class|nested_class|enum|record|interface",
      "captures_outer": true, "outer_fields_captured": [ "config" ] }
  ],
  "edges": {
    "method_to_method": [
      { "from": "refreshGrid", "to": "getMeterGrid", "context": "direct|lambda" }
    ],
    "method_to_field": [
      { "method": "refreshGrid", "field": "meterGrid", "kind": "read|write" }
    ],
    "method_to_inherited": [
      { "from": "applyFilters", "to": "BaseView.applyFilters", "source_kind": "class|interface" }
    ]
  },
  "annotations_class_level": [ "Slf4j", "Route" ]
}
```

**Why "annotations_class_level" matters:** the existing
`extract_java_class` safety rules call out annotation-processor-
generated members as invisible to dependency analysis (`@Slf4j` →
`log`, `@Data` → accessors). A standalone graph atom should surface
class-level annotations so the operator knows which generated members
the partition decision needs to account for; the report is read by
humans before they commit to a partition, so this is the right place
for that hint.

**Implementation note:** the underlying tree-sitter walk already
exists. `extract_java_class` with `deep_analysis: true` runs it; the
new plan kind would be a refactored, non-mutating entry point that
returns just the analysis output without requiring extraction inputs.
Realistically a couple-hundred-line extraction.

**Atom that unblocks once shipped:** `java-class-dependency-graph`
(parallel to RA-A1), cost_class: cheap, parallel_safe: true.

---

## What ships without closing the gaps

The Java atom catalog still has four useful atoms even with both gaps
open:

1. `java-extract-cohesive-class` (RA-X1) — wraps `extract_java_class`.
2. `java-promote-inner-class` (RA-X2) — wraps `promote_java_inner_class`.
3. `java-extract-interface` (RA-X3) — wraps `extract_java_interface` +
   optional `migrate_java_type_usages`. Operator-authority on
   `acknowledge_public_api_change` is **prompt-discipline only** until
   Gap 1 closes — the atom can't run a structured preflight, only ask
   the operator to confirm.
4. `java-lombokify` (RA-X4) — wraps `lombokify_java_class`.

Plus the prerequisite `java-refactor-persona` brofile (RA-B2) and three
composition workflows (`promote-inner-then-extract`,
`extract-interface-and-migrate`, `pojo-modernize`).

## Recommended landing order for the gaps

```
1. java_public_api_guard plan kind
   └── unblocks structured preflight for java-extract-interface
   └── unblocks java-public-api-guard atom (standalone audit)

2. java_class_dependency_analysis plan kind
   └── unblocks java-class-dependency-graph atom (analysis-only preflight)
   └── unblocks operator-review composition (graph → extract)
```

Both gaps are pure additions — no existing plan-kind behavior changes,
no migrations needed. Each closes one Codex-round-1-style "the atom
can't honestly enforce its contract without this" hole.

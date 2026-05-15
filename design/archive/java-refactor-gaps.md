# Java Refactor Gap Inventory: Closed

Archived 2026-05-15.

This document was the working inventory for Java refactor tooling gaps. It is
closed: every Java refactor plan kind in the dispatch table now has a shipped
refactor atom wrapper, the inline `Gap N` regressions listed in the original
inventory have code-backed tests, and stale note-store claims from the draft
were rechecked instead of carried forward.

## Dispatch Surface

`bbox_refactor_plan(kind="...")` dispatches 20 Java plan kinds:

| Plan kind | Shipped atom coverage |
|-----------|-----------------------|
| `extract_java_class` | `java-extract-cohesive-class` |
| `extract_java_methods` | `java-extract-methods-light` |
| `extract_java_nested_classes` | `java-extract-static-nested-class` |
| `extract_java_interface` | `java-extract-interface` |
| `promote_java_inner_class` | `java-promote-inner-class` |
| `add_java_fields` | `java-add-fields` |
| `add_java_constructor` | `java-add-constructor` |
| `add_java_delegate_field` | `java-add-delegate-field` |
| `add_java_implements` | `java-add-implements` |
| `move_java_field` | `java-move-field` |
| `move_java_constant` | `java-move-constant` |
| `update_java_callers` | `java-update-callers` |
| `rewrite_java_visibility` | `java-rewrite-visibility` |
| `rename_java_symbol` | `java-rename-symbol` |
| `migrate_java_type_usages` | `java-migrate-type-usages` |
| `lombokify_java_class` | `java-lombokify` |
| `find_java_usages` | `java-find-usages` |
| `java_class_dependency_analysis` | `java-class-dependency-graph` |
| `java_public_api_guard` | `java-public-api-guard` |
| `java_lsp_organize_imports` | `java-organize-imports` |

The guard test
`java_refactor_plan_kinds_have_atom_coverage` enforces that each plan kind above
continues to appear in at least one shipped Java refactor atom prompt template.

## Atom Coverage Added In Closure Pass

The closure pass added nine atom manifests:

- `java-rewrite-visibility`
- `java-migrate-type-usages`
- `java-add-fields`
- `java-add-constructor`
- `java-add-delegate-field`
- `java-add-implements`
- `java-move-field`
- `java-move-constant`
- `java-update-callers`

Each was added under `system-defaults/atoms/refactor/` and covered in the
deterministic atom eval catalogs:

- `eval/atoms/refactor/discovery-queries.json`
- `eval/atoms/refactor/dispatch-scenarios.json`
- `eval/atoms/refactor/behavior-smoke.json`

## Inline Gap Markers

The original inventory catalogued 27 distinct inline `Gap N` markers across
`src/refactor/java/*.rs` (numbers 1-29, with 9 and 15 unused). Those markers are
now implementation history plus regression-test labels, not open design work.
Representative covered areas include:

- Lombok boolean accessor compatibility.
- Cross-file static caller rewrites.
- Inner-type qualification for cross-package extraction.
- Capture shadowing, mutability, and static-final constant handling.
- Generated FIXME scaffolding for unresolved external/inherited dependencies.
- Visibility floors for delegate-rewritten methods.
- Import pruning, wildcard handling, and target-package derivation.
- Existing-target append behavior for method extraction.
- `PlanStatus` serialization alignment.

The Java regression suite keeps those behaviors covered in
`src/refactor/java/tests.rs`.

## Note Store Recheck

The initial draft carried note-store claims that had not been checked against
the store. During closure, unresolved followup notes matching `java refactor
gap` for this project were queried and none were found. The fabricated note IDs
from the initial draft remain intentionally absent.

## Cross-Language Disposition

The draft also contained a scratch cross-language comparison. It is not an open
Java gap list:

- `extract_java_interface` has a Rust counterpart in `extract_rust_trait`.
- Java field moves accept multiple `item_names`; Rust has
  `move_rust_struct_fields` for the struct-specific shape.
- `lombokify_java_class` and `rewrite_rust_error_type` are language-specific
  refactor families rather than missing Java plan-kind coverage.

This archive therefore closes the Java refactor gap inventory without carrying
forward cross-language comparison notes as active Java gaps.

## Validation

Closure was validated with:

```text
jq empty system-defaults/atoms/refactor/java-*.json \
  eval/atoms/refactor/discovery-queries.json \
  eval/atoms/refactor/dispatch-scenarios.json \
  eval/atoms/refactor/behavior-smoke.json

cargo test --lib java_refactor_plan_kinds_have_atom_coverage --no-fail-fast
cargo test --lib every_shipped_refactor_atom_passes_atom_validation --no-fail-fast
cargo test --lib refactor_atom_eval_suites_cover_every_shipped_atom --no-fail-fast
```

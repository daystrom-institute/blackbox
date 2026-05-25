---
title: "Code Navigation Depth — Axis 1 (symbol resolution)"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - code-navigation
tags:
  - code-navigation
  - refactor-tools
brief: "Containing-symbol resolution + refactor-item reachability fixes that landed, and the deferred language/depth-aware chunker symbol-emission redesign."
---

# Code Navigation Depth — Axis 1 (symbol resolution)

Date: 2026-05-24
Status: core fixes landed (commit `864a5c10`); chunker symbol-emission expansion
deferred. Companion to `code-nav-symbolic-exploration.md` and its impl skeleton.

## Context

`bbox_code_*` (`src/code_nav/mod.rs`, `src/tools/code_nav.rs`) are syntax-only
tree-sitter surfaces. A depth-improvement pass ("Axis 1") was originally framed
as "kill the Rust-only synthesis ceiling" — expand `refactor_kind_for`
(`src/code_nav/mod.rs`) into a per-language synthetic-kind table (e.g. Java
`interface_method`, `constructor`). Closer reading of the code reversed that
framing: the synthesis table should NOT be expanded, and the genuinely safe
depth was elsewhere.

## Finding 1 — do not expand the synthetic-kind table

The dual-lane contract: the indexed `code_symbols` lane derives `refactor_kind`
from `(language, symbol_kind, parent_kind)` via `refactor_kind_for`; the live
lane gets `refactor_kind` natively from `refactor::status`. An equivalence test
pins that the two lanes agree
(`src/code_nav/tests.rs::indexed_lane_item_kinds_matches_both_synthetic_and_raw_for_rust_impl_method`).

Established by reading the code:

1. The live lane emits **raw tree-sitter kinds for everything** —
   `generic_top_level_items` → `syntax_item` → `kind: node.kind()`
   (`src/refactor/mod.rs`); Java methods/classes go through
   `syntax_item_with_kind(parsed, child, kind)` with the raw kind
   (`src/refactor/java.rs`).
2. The ONLY synthetic kind anywhere is Rust `impl_method`, and it exists because
   Rust has refactor PLAN kinds that operate on impl-block methods which are not
   top-level nodes (`extract_rust_impl_methods`, `delete_rust_impl_methods`,
   `move_rust_items_with_local_deps`).
3. `refactor_kind_for`, `indexed_kind_filter_for`, and `symbol_kind_from_refactor`
   all hardcode the single `impl_method` case and must stay mirrored.

Precise claim: no additional *code-nav / refactor-status synthesis* consumer
exists beyond Rust `impl_method`. (This is distinct from "no plan-local
`item_kinds` vocabulary exists" — Java refactor plans do use plan-local
vocabularies like `field` / `method`, but those never flow through
`refactor_kind_for` or the indexed `symbol_kind` lane.) Adding
`interface_method` / `constructor` synthesis would require a coupled change in
`refactor::status` live emission to keep equivalence, and would invent a kind
vocabulary no plan consumes. **Conclusion: leave synthesis at the single
justified case.**

## Root cause — `is_symbol_node` is the wrong lever for indexed depth

Several tempting "add a kind" changes were investigated and rejected for one
shared reason. Chunker `is_symbol_node` (`src/chunker/code.rs`) is a **global,
kind-only** predicate, and `collect_ast_symbols` emits matches at **any depth**.
Live `refactor::status` emission is **language- and depth-aware**: special
collectors for Rust (root top-level items + impl methods) and Java, and a
top-level-only generic fallback (`generic_top_level_items` →
`root.named_children`) for everything else.

Therefore any kind added to `is_symbol_node` risks new indexed-only symbols the
live lane never emits:

- **cross-language**: `constructor_declaration` and `record_declaration` are
  shared by `tree-sitter-java` and `tree-sitter-c-sharp`; C# has no special live
  collector, so adding them globally indexes C# constructors/records the live
  lane never produces.
- **intra-language by nesting**: Rust's `_declaration_statement` supertype lets
  `const_item`, `static_item`, `macro_definition`, `type_item` appear as LOCAL
  items (in fn bodies) or ASSOCIATED items (in impl/trait blocks); the recursive
  chunker would index those, live status would not.

Closing the indexed-vs-live emission gap is real work, but it needs a deliberate
emission-scoping design (depth/top-level gating in the chunker, or intentional
matching live emission per language with per-language equivalence fixtures) —
not a one-line predicate tweak.

## Landed in this pass (commit `864a5c10`)

Two collision-free fixes with zero chunker/live equivalence impact:

1. **`containing_symbol_for`** (`src/code_nav/mod.rs`, drives
   `bbox_code_refs.containing_symbol`):
   - Replaced a hand-copied symbol-kind subset with one canonical predicate,
     `is_containing_symbol_kind`, derived from the now-`pub(crate)`
     `chunker::code::is_symbol_node` plus `impl_item`
     (which `is_symbol_node` does not list — it is special-cased in
     `symbol_name`), minus parser roots (`is_root_kind`), `package_declaration`
     (a symbol node but not a parser root, so `is_root_kind` does not exclude
     it — a package is not a containing scope), and `field_declaration` (a
     member, not an enclosing scope). This recovers kinds the old subset
     silently missed (`method_spec`, `mod_item`, `struct_specifier`, …).
   - Fixed the walk loop: a matched-but-nameless container (e.g. an anonymous
     Go `interface_type`) no longer terminates the walk with `None` — it keeps
     climbing to the nearest named symbol. The Rust `impl_item` header fallback
     is preserved.

2. **`is_refactor_item_kind`**: added `record_declaration`. The Java record
   branch in `refactor_status_item` already matched it but sat behind the
   `is_refactor_item_kind` guard, so it was unreachable; Java records now
   resolve as refactor items.

Tests: `is_containing_symbol_kind_contract`,
`containing_symbol_for_climbs_past_nameless_container` (Go),
`java_record_resolves_as_refactor_item`.

## Deferred (the `partial` remainder)

Design a depth/language-aware chunker symbol-emission rule that keeps the indexed
lane equivalent to live emission, then add — under that rule, with per-language
equivalence fixtures:

- Rust top-level `const_item` / `static_item` / `macro_definition` / `type_item`
  emission (top-level only, mirroring `is_top_level_item`).
- Java `constructor_declaration` / `record_declaration` emission, with C# either
  given its own live collector or explicitly accounted for.

Until then, those items remain visible on the live lane / refactor plans but not
in the indexed `code_symbols` / `bbox_refactor_project_refs` surfaces.

Also deferred: full consolidation of the three overlapping symbol-kind predicates
(`is_symbol_node`, `is_refactor_item_kind`, `is_containing_symbol_kind`) into one
source of truth. They have genuinely different container/root semantics;
collapsing them risks behavior change in the code-nav handoff path.

## Review trail

Reviewed across four proposal rounds plus two implementation rounds by a codex
gpt-5.5 (high effort) reviewer. The review caught the `package_declaration`
exclusion gap, the nameless-container walk-halt bug, the C# `constructor_declaration`
collision, and the Rust nesting hazard — each of which reshaped the scope above.
Final implementation review: APPROVE.

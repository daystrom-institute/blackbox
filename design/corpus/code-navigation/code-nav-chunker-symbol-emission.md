---
title: "Code Navigation — Language/Depth-Aware Chunker Symbol Emission"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - code-navigation
tags:
  - code-navigation
  - refactor-tools
  - chunker
brief: "Make the indexed code_symbols lane equivalent to live refactor::status by scoping chunker-emitted symbols per language and nesting depth, without regressing search/embedding recall."
---

# Code Navigation — Language/Depth-Aware Chunker Symbol Emission

Date: 2026-05-25
Status: proposed. Realizes the deferred remainder of
`code-nav-depth-axis1.md` ("the `partial` remainder").

## Problem

`bbox_code_symbols(mode="indexed")` reads symbols the **chunker** emitted into
tantivy. `bbox_code_symbols(mode="live")` and the refactor plan family read
symbols the **live** `refactor::status` walker produces. The indexed lane is
supposed to be a fast, parse-free equivalent of the live lane (CN-T2 contract:
"`mode="indexed"` returns the same logical items as `mode="live"` for the same
query"). (`bbox_refactor_project_refs` is neither of these — it re-chunks the
current file live; see Consumer impact.)

These two emitters disagree, and that is why several useful kinds
(`const_item`, `static_item`, `macro_definition`, `type_item`,
`constructor_declaration`, `record_declaration`) are **not** surfaced by the
indexed lane today: `chunker::code::is_symbol_node` simply omits them. The Axis 1
spike (`code-nav-depth-axis1.md`) established that naively adding them to
`is_symbol_node` trades one defect for another, because:

- **The chunker is global, kind-only, and emits at every depth.**
  `collect_ast_symbols` (`src/chunker/code.rs`) emits a `SymbolSpec` for any node
  where `symbol_name` returns a name, recursing through all named children.
- **Live emission is language- and depth-aware.** The dispatcher
  (`src/refactor/mod.rs:1104`) routes:
  - `rust` → `rust_status_items` = top-level items (`rust_items`) **plus** impl
    methods (`function_item` under a top-level `impl_item`).
  - `java` → `java_status_items` = top-level items **plus** methods/constructors
    at any class-nesting depth (`java_methods` walks into class bodies) **plus**
    nested classes (`java_nested_classes`).
  - everything else (C#, Go, Python, …) → `generic_top_level_items` =
    `root.named_children` only (**top-level only**).

So a kind added globally to `is_symbol_node` becomes an indexed-only symbol
wherever the live walker would not have emitted it:

- **cross-language**: `constructor_declaration` / `record_declaration` are shared
  by `tree-sitter-java` and `tree-sitter-c-sharp`; C# has no special live walker,
  so a C# constructor nested in a class is emitted by the chunker but never by
  live (generic = top-level only).
- **intra-language by nesting**: Rust's `_declaration_statement` supertype lets
  `const_item` / `static_item` / `macro_definition` / `type_item` appear as
  **local** items (in fn bodies) or **associated** items (in impl/trait blocks);
  the chunker emits those, live status does not.

## Partial signal — `parent_kind` helps but is not sufficient

`SymbolSpec.parent_kind` is the kind of the **nearest enclosing symbol frame**,
not the raw AST parent (`collect_ast_symbols`, `src/chunker/code.rs:351`, pushes a
`SymbolStackFrame` per emitted symbol; `parent_kind = stack.last()`).
`symbol_kind`, `parent_kind`, and `language` are already stored/queryable in the
tantivy schema (`src/index/mod.rs:76,657`, `g7`).

`parent_kind` alone, however, **cannot** reconstruct live scoping, because it
collapses the ancestor chain to one frame (review R1, MF1/MF2):

- **Rust impl-in-mod.** `rust_impl_methods` only walks `root.named_children`
  impls (`src/refactor/rust.rs:2347`), so live does NOT emit methods of an impl
  nested inside a `mod`. But such a method still has `parent_kind = impl_item`,
  indistinguishable from a top-level impl method. A `parent_kind`-only rule would
  wrongly include it → CN-T2 break, not a documentable superset.
- **Java field leak.** A kind-agnostic "member of a type" rule would admit
  `field_declaration` (already a chunker symbol kind, `src/chunker/code.rs:412`),
  which Java live status does NOT emit (`java_status_items` adds only
  methods/constructors via `java_methods` and nested type declarations via
  `java_nested_classes`, `src/refactor/java.rs:38,75,116`).

So the predicate needs (a) the **enclosing symbol-kind chain** (root→parent), not
just the nearest frame, and (b) **kind-specificity**.

## Design — enclosing-chain signal + kind-specific scope predicate

Two parts.

### 1. Carry the enclosing-symbol-kind chain into the index

`collect_ast_symbols` already holds the full `stack: Vec<SymbolStackFrame>`. Emit
the chain of enclosing symbol kinds (root→parent) on each `SymbolSpec`, plumb it
through `Chunk` to a new stored tantivy field `enclosing_kinds` (e.g. a
`/`-joined token string such as `"impl_item"`, `"mod_item/impl_item"`,
`"class_declaration/class_declaration"`; empty for top-level). This is a real new
field, which the parent_kind-only framing wrongly claimed to avoid — the R1
review showed nearest-frame is insufficient. Cost is justified: a forced reindex
is already required to backfill the broadened kinds (see Migration), so the field
rides along on the same reindex.

`parent_kind` stays (it is the chain's last element and is still used by
`refactor_kind_for` synthesis); `enclosing_kinds` is additive.

### 2. Centralized, kind-specific scope predicate

```rust
/// True when an indexed symbol record is one the live `refactor::status`
/// walker for that language would also emit. Single source of truth for
/// indexed↔live equivalence; mirrors refactor::{rust,java,mod} scoping.
/// `enclosing_kinds` is the root→parent chain of enclosing symbol kinds.
pub fn live_equivalent_scope(
    language: &str,
    symbol_kind: &str,
    enclosing_kinds: &[&str],
) -> bool
```

Semantics (mirrors the dispatcher at `src/refactor/mod.rs:1104`):

| language | rule |
|----------|------|
| `rust`   | chain empty (top-level item, gated by `is_top_level_item`) OR (`symbol_kind == "function_item"` AND chain `== ["impl_item"]` — method of a **top-level** impl; chain `["mod_item","impl_item"]` is excluded) |
| `java`   | chain empty (top-level) OR (`symbol_kind ∈ {method_declaration, constructor_declaration}` AND every chain element ∈ the type-declaration set) OR (`symbol_kind ∈ {class_declaration, interface_declaration, record_declaration, enum_declaration}` AND every chain element ∈ the type-declaration set — a nested type at any class depth). `field_declaration` is NOT admitted. "Every element a type declaration" excludes members nested under a method body. |
| _other_  | chain empty (top-level only — matches `generic_top_level_items`; C# nested constructors excluded) |

The "every chain element is a type declaration" test is what `parent_kind` alone
could not express and is why the chain field is required.

Application points:

1. **`code_symbols` indexed lane** (`code_nav::code_symbols_indexed`): apply
   `live_equivalent_scope` as a **post-filter** on candidate records (see Fetch
   semantics below). Computing at query time keeps the rule evolvable without a
   reindex.
2. **`bbox_refactor_project_refs`**: do NOT apply the filter by default (see
   Consumer impact — it is a live re-chunk/grounding tool, not an indexed read).

`is_symbol_node` then **broadens** to include `const_item`, `static_item`,
`macro_definition`, `type_item`, `constructor_declaration`, `record_declaration`.
Broad emission is correct here: the tantivy index and embeddings get these
symbols at all depths (a recall win for search — local consts, macros, nested
constructors all become findable), while the equivalence-bound surfaces apply
`live_equivalent_scope` to match live.

This decouples the two contracts that were wrongly coupled through one predicate:
- **search/embedding emission** = broad (all depths, all the new kinds);
- **`code_symbols` indexed emission** = live-equivalent subset (scope post-filter).

### Fetch / count semantics (review R1, MF3)

`code_symbols_indexed` already distinguishes a `has_post_filter` path from the
exact path via a `Count` collector for `total_hits` (`src/code_nav/mod.rs:1434`):
with a post-filter, `fetch_cap = total_hits.min(INDEXED_SCAN_CAP)` and it may
report `truncation_reason="scan_cap_reached"`; without one,
`fetch_cap = limit.saturating_mul(2).max(64).min(total_hits)` (headroom over
`limit`, not `limit` itself) and `total_hits` gives an exact count
(`src/code_nav/mod.rs:1435-1442`). `live_equivalent_scope` **is a post-filter**
and must be wired into that path: force `has_post_filter = true` whenever
broadened kinds can appear, so the lane over-fetches to `INDEXED_SCAN_CAP`,
computes `matching_items` after scope filtering, and reports `scan_cap_reached`
honestly. Otherwise valid live-equivalent records can be hidden behind
filtered-out broad symbols within the smaller no-filter fetch headroom.

## Why not the alternatives

- **Chunker-side scope gating** (emit only live-equivalent symbols): loses search
  recall for nested items and pushes per-language refactor scoping into the
  language-agnostic chunker. Rejected.
- **Materialize a boolean `live_scope` field at index time** instead of computing
  from `enclosing_kinds`: freezes the rule until the next reindex. Storing the raw
  `enclosing_kinds` chain and computing the predicate at query time keeps the rule
  evolvable while still carrying the minimal signal the predicate provably needs.
  Preferred.
- **`parent_kind`-only, accept superset**: rejected — the Rust impl-in-mod case is
  a CN-T2 equivalence break, not a benign superset (see above).

## Known precision limits (with the chain signal)

With `enclosing_kinds` the two previously-unresolvable cases are now decidable:

1. **Rust impl-in-mod.** chain `["mod_item","impl_item"]` ≠ `["impl_item"]`, so the
   method is correctly excluded — matches live.
2. **Java method-body locals.** A local type under a method body has a chain
   containing `method_declaration`, failing "every element is a type
   declaration" — correctly excluded.

Residual edge to pin in tests, not hide: the `tree_sitter_language_pack` fallback
path (`structure_symbol_specs`, for languages outside the curated AST grammar)
produces abstract kind labels, not tree-sitter node kinds, and its chain will use
those labels. `live_equivalent_scope` for those languages falls under the `_other_`
top-level-only rule, which is chain-empty — so fallback nesting is excluded
regardless of label vocabulary. The equivalence test must include one
fallback-language fixture to lock this.

## Consumer impact

- **Search / embeddings**: additive recall for the new kinds. **Not** strictly
  free, though (review R1, MF5): `chunks_from_symbols` assigns `occurrence_idx` by
  enumeration over byte-sorted specs (`src/chunker/code.rs:189`), and
  `project_refs` embeds that index into entity refs
  `project_file:{project_id}:{rel_path_hash}:{chunk_hash}:{occurrence_idx}`
  (`src/refactor/mod.rs:1169`). Inserting nested symbols shifts `occurrence_idx`
  for later chunks in the same file, so the forced reindex **churns existing
  `project_file` entity refs** for any file that gains a new symbol. This must be
  documented as a migration effect; a follow-up could stabilize `occurrence_idx`
  against insertion (e.g. derive it from `byte_start` rather than dense
  enumeration), but that is out of scope here.
- **`code_symbols` indexed**: now returns const/static/macro/type (Rust,
  top-level) and constructors/records (Java, type members), equivalent to live.
- **`code_symbols` live**: unchanged; already correct by construction.
- **`bbox_refactor_project_refs`** (review R1, MF4): it is NOT an indexed read —
  it re-chunks the current file (`chunk_file_for_refs`, `src/refactor/mod.rs:1146`)
  and returns current `project_file` entity refs for grounding
  (`src/tool_docs.rs:405`). Applying `live_equivalent_scope` by default would drop
  the newly-broad chunks it is meant to surface for grounding. Default: **do not
  filter**. If a scoped view is ever wanted, add it behind an explicit
  opt-in parameter/mode — not as default behavior.
- **`refactor_kind_for` synthesis**: unchanged — still the single `impl_method`
  case. Scope filtering is orthogonal to kind synthesis.

## Migration

Two coupled index changes ride one reindex:
- new stored field `enclosing_kinds` (schema change);
- broadened `is_symbol_node` emission (new symbol rows).

Bump `INDEX_SCHEMA_VERSION` (`agentic-corpus-g7-...` → `g8-...`).
`reset_index_on_schema_mismatch` compares only the version marker
(`src/index/mod.rs:15,694`), so the bump both registers the new field and forces
the one-time backfill reindex, mirroring the CN-D3 precedent. Document in the
agentic-corpus release notes and `bbox_embed_status`: (a) the one-time reindex
cost, and (b) the `project_file` entity-ref churn from `occurrence_idx` shifts so
downstream ref consumers expect it.

## Equivalence test plan

- Unit: `live_equivalent_scope(language, symbol_kind, enclosing_kinds)` truth
  table — Rust: top-level item / impl-method (chain `["impl_item"]`) /
  impl-in-mod excluded (chain `["mod_item","impl_item"]`) / associated-const
  excluded / fn-local excluded; Java: member method / member constructor /
  nested class (chain all type-decls) / `field_declaration` excluded /
  method-body-local excluded (chain contains `method_declaration`); C#
  nested-constructor excluded; generic top-level-only; one
  language-pack-fallback fixture (chain-empty top-level only).
- Cross-lane fixture: a project containing each case, asserting the
  `(file, byte_start, byte_end, symbol_kind, refactor_kind, name)` set from
  `code_symbols(mode="indexed")` equals `code_symbols(mode="live")` after a
  reindex — extending the existing
  `indexed_lane_item_kinds_matches_both_synthetic_and_raw_for_rust_impl_method`
  test from impl-method-only to the full new kind set.
- Fetch/count: a file with many filtered-out broad symbols plus a few
  live-equivalent ones confirms `matching_items`, truncation, and the
  over-fetch cap behave correctly under the post-filter path.
- Search regression smoke: a nested Rust `const` is findable via
  `bbox_hybrid_search` but is NOT returned by `code_symbols(mode="indexed")`.

## Non-goals

- Per-language synthetic `refactor_kind` vocabularies beyond `impl_method`
  (settled in `code-nav-depth-axis1.md` Finding 1).
- A C# live `refactor::status` walker. C# stays on the generic top-level-only
  scope until a separate effort adds one; the predicate already encodes that.
- Consolidating the three symbol-kind predicates (`is_symbol_node`,
  `is_refactor_item_kind`, `is_containing_symbol_kind`) into one — tracked
  separately; they retain distinct container/root semantics.
- Stabilizing `occurrence_idx` against symbol insertion (to avoid the entity-ref
  churn noted under Consumer impact) — a worthwhile follow-up, but out of scope.

## R1 review disposition (codex gpt-5.5 high, session 019e5c22)

- MF1 (parent_kind cannot satisfy CN-T2; Rust impl-in-mod is a break, not a
  superset) → adopted the `enclosing_kinds` chain field; predicate now decides
  impl-in-mod correctly. Dropped the "no new field" framing.
- MF2 (Java rule too broad, admits `field_declaration`) → predicate is now
  kind-specific (methods/constructors/nested type declarations only) with an
  "every chain element is a type declaration" guard.
- MF3 (post-filter vs fetch caps/counts/truncation) → added the Fetch/count
  semantics section wiring `live_equivalent_scope` into the `has_post_filter`
  path.
- MF4 (`project_refs` is a live re-chunk, not an indexed read) → corrected;
  default is no filter, optional opt-in only.
- MF5 (`occurrence_idx` churn from broad emission + reindex) → documented under
  Consumer impact and Migration; stabilization listed as a follow-up non-goal.

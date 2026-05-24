# Axis 1 spike proposal — code-nav depth (for codex 5.5 review)

Status: PROPOSAL for review. Not yet implemented.
Scope owner: main-session Claude. Reviewer: codex gpt-5.5 high.

## Background

`bbox_code_*` (`src/code_nav/mod.rs`, `src/tools/code_nav.rs`) are syntax-only
tree-sitter surfaces. The original Axis 1 framing was "kill the Rust-only
synthesis ceiling" — i.e. expand `refactor_kind_for` (`src/code_nav/mod.rs:56`)
into a per-language synthetic-kind table (e.g. Java `interface_method`,
`constructor`). On closer reading of the code I now believe **that part of Axis 1
is wrong and should NOT be done.** This proposal explains why, and replaces it
with two changes that have actual consumers.

I want you to red-team BOTH the reversal and the replacement scope.

## Finding 1 — synthesis-table expansion is unjustified (REVERSAL)

The dual-lane contract: the indexed `code_symbols` lane derives `refactor_kind`
from `(language, symbol_kind, parent_kind)` via `refactor_kind_for`
(`src/code_nav/mod.rs:56`); the live lane gets `refactor_kind` natively from
`refactor::status`. An equivalence test asserts the two lanes agree
(`src/code_nav/tests.rs:672` `indexed_lane_item_kinds_matches_both_synthetic_and_raw_for_rust_impl_method`).

Key facts established by reading the code:

1. The live lane emits **raw tree-sitter kinds for everything**. `generic_top_level_items`
   → `syntax_item` → `kind: node.kind()` (`src/refactor/mod.rs:3962-3983`). Java
   methods/classes go through `syntax_item_with_kind(parsed, child, kind)` with
   `kind` = raw tree-sitter kind (`src/refactor/java.rs:75-80, 121-125`).
2. The ONLY synthetic kind anywhere is Rust `impl_method`, and it exists because
   Rust has refactor PLAN kinds that operate on impl-block methods which are not
   top-level nodes: `extract_rust_impl_methods`, `delete_rust_impl_methods`,
   `move_rust_items_with_local_deps` (rejects it) — `src/refactor/rust.rs:252,360,615`.
3. `refactor_kind_for` and `indexed_kind_filter_for` (`src/code_nav/mod.rs:95`)
   and `symbol_kind_from_refactor` (`:69`) ALL hardcode the single `impl_method`
   case and must stay mirrored. The equivalence test enumerates it.

Therefore: adding `interface_method`/`constructor`/etc. synthesis would require a
**coupled change in `refactor::status` live emission** (java.rs etc.) to keep the
equivalence test green, AND it would invent a kind vocabulary that **no refactor
plan kind consumes** — purely cosmetic, pure churn, new drift surface. The reason
`impl_method` is synthesized is that a plan needs it; nothing analogous exists for
other languages today.

**Proposed conclusion: do not expand `refactor_kind_for`. Leave synthesis at the
single justified case.** Precise claim (tightened per R1 review): no additional
*code-nav / refactor-status synthesis* consumer exists beyond Rust `impl_method`.
This is NOT the same as "no plan-specific `item_kinds` vocabulary exists" — Java
refactor plans do use plan-local vocabularies like `field` / `method` / usage
categories. Those are plan-local and do not flow through `refactor_kind_for` or
the indexed `symbol_kind` lane, so they are out of scope for a synthesis table.

## Finding 2 — `is_symbol_node` omits raw kinds the live lane already emits (REAL GAP)

There are THREE symbol-kind predicates in play (R1 review surfaced the third):
- `is_symbol_node` (`src/chunker/code.rs:412`) — chunker symbol-emission; drives
  what indexed `code_symbols` / `bbox_refactor_project_refs` can find.
- `is_refactor_item_kind` (`src/code_nav/mod.rs:742`) — code-nav handoff /
  nearest-refactor-item; a strict SUPERSET that already lists `const_item`,
  `static_item`, `macro_definition`, `constructor_declaration`,
  `struct_specifier`, `interface_type`, `impl_item`, `type_item`, etc.
- `is_top_level_item` (`src/refactor/mod.rs:3995`) — refactor live top-level set;
  lists `macro_definition`, `const_item`, `static_item`, `type_item`.

The chunker predicate is the laggard. Confirmed missing from `is_symbol_node`
but present in the others / emitted live:

- **Rust**: `const_item`, `static_item`, `macro_definition`, `type_item`. Live
  lane emits them (`is_top_level_item`); `rust_visibility_keyword_byte` handles
  them (`src/refactor/rust.rs:2130-2140`). Invisible to the indexed lane today.
- **Java**: `constructor_declaration`. Emitted live by `java_status_items` /
  `walk_java_methods` (`src/refactor/java.rs:38,75`) and listed in
  `is_refactor_item_kind`, but absent from chunker `is_symbol_node`. Same
  indexed-vs-live divergence (R1 finding).

Separately, R1 found a latent bug: `refactor_status_item`
(`src/code_nav/mod.rs:813`) has a Java branch matching `record_declaration`
(`:836`), but `record_declaration` is NOT in `is_refactor_item_kind`
(`:742-769`), so the outer `if is_refactor_item_kind(...)` guard (`:820`) means
that branch is **unreachable**. Java records never resolve as a refactor item.

### The cross-language hazard (R2 finding — reshapes change 2)

Chunker `is_symbol_node` is **kind-only / language-agnostic**, and
`collect_ast_symbols` walks recursively. But live `refactor::status` is
**language-aware**: special collectors for Rust and Java, and a
top-level-only generic fallback (`generic_top_level_items` →
`root.named_children`, `src/refactor/mod.rs:3954`) for everything else.

Consequence: any kind I add to `is_symbol_node` that (a) is shared by a
supported grammar with no special live collector, or (b) appears nested rather
than top-level in such a grammar, creates a NEW indexed-vs-live divergence for
that language. R2 confirmed `constructor_declaration` AND `record_declaration`
are present in BOTH `tree-sitter-java` and `tree-sitter-c-sharp`. C# has no
special live collector, so adding those globally would index C# constructors /
records that the live lane never emits. That trades one divergence for another.

### The spike finding (R3 — `is_symbol_node` is the wrong lever)

Three review rounds converged on one root cause: **chunker `is_symbol_node` is a
global, kind-only predicate and `collect_ast_symbols` emits matches at ANY depth
(`src/chunker/code.rs:344`), while live `refactor::status` emission is
language-aware and depth-aware** (Rust: root top-level items + impl methods,
`src/refactor/rust.rs:2328-2363`; generic fallback: top-level only,
`src/refactor/mod.rs:3954`). Therefore ANY kind added to `is_symbol_node` risks
new indexed-only symbols the live lane never emits:

- cross-language (R2): `constructor_declaration` / `record_declaration` are
  shared with C#, which has no special live collector;
- intra-language by nesting (R3): the Rust `_declaration_statement` supertype
  lets `const_item`, `static_item`, `macro_definition`, `type_item` appear as
  LOCAL items (in fn bodies) or ASSOCIATED items (in impl/trait blocks); the
  recursive chunker would index those, live status would not.

Closing the indexed-vs-live emission gap is real work, but it requires a
deliberate emission-scoping design (depth/top-level gating in the chunker, or
intentional matching live emission per language) — NOT a one-line predicate
tweak. That is bigger than a spike and is the wrong thing to ram in. So:

### Proposed change 2 (rescoped: defer ALL chunker emission)

**DEFER — chunker `is_symbol_node` emission changes (both Rust and Java).**
Documented follow-up: design a depth/language-aware symbol-emission rule that
keeps the indexed lane equivalent to live emission, then add the Rust top-level
const/static/macro/type and the Java constructor/record cases under that rule
with per-language equivalence fixtures. Not in this spike.

**KEEP — the one code-nav handoff fix that does NOT touch chunker/live
equivalence:**
- Add `record_declaration` to `is_refactor_item_kind` (`src/code_nav/mod.rs:742`)
  to make the existing — currently unreachable — Java record branch in
  `refactor_status_item` (`:829`) reachable. Pure code-nav handoff resolution; no
  chunker emission, no cross-language divergence.
- Test: code-nav test asserting a Java `record_declaration` now resolves via
  `nearest_refactor_item` / `refactor_status_item`.

## Finding 3 — `containing_symbol_for` is a divergent hand-copy of the predicate

`containing_symbol_for` (`src/code_nav/mod.rs:1655-1707`) walks ancestors to find
the enclosing symbol for a `bbox_code_refs` record. It hardcodes its OWN subset of
symbol-node kinds (`:1669-1684`):

```
function_item, function_definition, function_declaration, method_definition,
method_declaration, impl_item, class_definition, class_declaration, struct_item,
enum_item, enum_declaration, trait_item, interface_declaration
```

Compare `is_symbol_node` (`src/chunker/code.rs:412`):

```
...method_spec, struct_specifier, field_declaration, interface_type, mod_item,
source_file, package_declaration, type_declaration, type_spec... (superset, mostly)
```

Divergences:
- `containing_symbol_for` MISSES: `method_spec` (Go), `struct_specifier` (C),
  `interface_type` (Go), `mod_item` (Rust nested mod), `type_declaration`/
  `type_spec` (Go). So a ref inside a Go method or a Rust nested `mod` fails to
  resolve its containing symbol.
- `is_symbol_node` INCLUDES container kinds that must NOT be reported as a
  "containing symbol": `source_file`, `package_declaration`, and arguably
  `field_declaration`. A naive "just call is_symbol_node" dedupe would make a
  top-level item report `source_file` as its container — a regression.

### Proposed change 3 (revised per R1 — shape A was buggy)

R1 found two defects in the naive shape A:
1. The exclusion set is wrong. It is not enough to exclude `source_file` /
   `package_declaration`. `field_declaration` must ALSO be excluded from
   containing-symbol results, and all root-ish kinds
   (`is_root_kind`: `source_file | program | module | translation_unit |
   compilation_unit`, `src/code_nav/mod.rs:796`) should be excluded — reuse that
   helper rather than hand-listing two.
2. **The walk-up logic itself is buggy.** `containing_symbol_for`
   (`src/code_nav/mod.rs:1655`) currently, on matching a symbol kind, tries the
   `name` field, then the rust-impl header fallback, then `return None`
   (`:1685,:1702`). So a matched-but-nameless container (e.g. Go `interface_type`,
   or any matched kind whose `name` child is absent) **halts the walk and yields
   `None`** instead of continuing upward to a usable named outer symbol. Adding
   more kinds to the matcher makes this worse, not better.

Revised shape A:

- Export a single canonical `is_symbol_node` from `chunker::code` and add a
  sibling `is_containing_symbol_kind(kind) -> bool`:
  `(is_symbol_node(kind) || kind == "impl_item") && !is_root_kind(kind) && !matches!(kind, "field_declaration" | "package_declaration")`.
  `impl_item` must be added explicitly — it is NOT in `is_symbol_node`'s
  `matches!` (it is special-cased in `symbol_name`, `src/chunker/code.rs:392`),
  but `containing_symbol_for` relies on it (`:1694`). **`package_declaration`
  must be excluded explicitly (R2 finding)**: it IS in `is_symbol_node`
  (`src/chunker/code.rs:432`) but is NOT a parser root, so `is_root_kind`
  (`src/code_nav/mod.rs:796`) does not cover it — without the explicit exclusion a
  Java package declaration would wrongly resolve as a containing symbol.
  `field_declaration` excluded for the same "not a containing scope" reason.
  (Note `is_root_kind` lives in `code_nav`; either move it to a shared spot or
  duplicate the small set with a comment — reviewer's call on placement.)
- **Fix the walk loop** so a matched kind with no resolvable name does NOT return
  `None` — it continues to `current = parent.parent()` and keeps climbing. Only
  return `None` when the walk reaches the root with no named symbol found. This
  is the load-bearing correctness fix; the predicate consolidation is secondary.

### Deferred (explicit, not silently dropped)

Full consolidation of the three predicates (`is_symbol_node`,
`is_refactor_item_kind`, `is_containing_symbol_kind`) into one source of truth is
NOT in this spike — they have genuinely different container/root semantics and
collapsing them risks behavior change in the code-nav handoff path. Tracked as
follow-up. (See FINAL IMPLEMENTED SCOPE below — chunker emission is deferred; this
spike ships only the `record_declaration` reachability fix and the
`containing_symbol_for` fix.)

### Tests for change 3

- `code_refs` on a Go fixture with a call inside a `method_declaration` resolves
  `containing_symbol` (previously missed — `method_spec`/Go not in old set).
- `code_refs` where the nearest matched ancestor is nameless: walk continues and
  resolves the next named outer symbol rather than returning `None`.
- Negative test: a top-level call does NOT report a root kind
  (`source_file`/`program`/…) or a `field_declaration` as container.

## Validation plan

```
cargo test --lib chunker
cargo test --lib code_nav
cargo test --lib refactor   # ensure no equivalence regression
cargo clippy
```

## R1 review disposition (codex gpt-5.5 high, session 019e5c22)

- Must-fix 1 (shape A buggy: impl_item, field_declaration/root exclusion,
  nameless-container walk halt) → addressed in revised Finding 3.
- Must-fix 2 (Java `constructor_declaration` indexed-vs-live gap +
  `record_declaration` unreachable branch) → folded into Finding 2 with
  language-aware collision analysis and explicit deferral of full predicate
  consolidation.
- Must-fix 3 (tighten Finding 1 wording; add equivalence tests for new Rust raw
  kinds + Java gaps) → Finding 1 reworded; tests enumerated in Findings 2 & 3.

## R2 review disposition (codex gpt-5.5 high, same session)

- MF1 (`package_declaration` not covered by `is_root_kind`) → added explicit
  `package_declaration` exclusion to `is_containing_symbol_kind` in Finding 3.
- MF2 (`record_declaration` also needs chunker emission for equivalence) →
  resolved by DEFERRING all Java chunker emission (constructor + record); the
  `record_declaration` change is kept ONLY as the `is_refactor_item_kind`
  reachability fix, which does not touch chunker/live equivalence. Record
  `SymbolSpec` chunker test removed; record test is now the code-nav reachability
  assertion only.
- MF3 (`constructor_declaration` is also a C# kind) → drove the rescope: chunker
  `is_symbol_node` additions are now restricted to the Rust-only collision-free
  set; Java/C#-shared kinds are explicitly deferred pending language-aware
  chunker detection.

## R3 review disposition (codex gpt-5.5 high, same session)

- MF1 (Rust kinds can appear nested via `_declaration_statement` → indexed-only
  symbols vs top-level-only live emission) → drove the final rescope: ALL chunker
  `is_symbol_node` emission changes are DEFERRED. Root cause documented as the
  spike finding ("`is_symbol_node` is the wrong lever"). The implemented spike is
  now exactly two code-nav-only fixes with zero chunker/live equivalence impact.

## FINAL IMPLEMENTED SCOPE

1. Fix `containing_symbol_for` (`src/code_nav/mod.rs:1655`): introduce exported
   `is_containing_symbol_kind` = `(is_symbol_node(kind) || kind=="impl_item") &&
   !is_root_kind(kind) && !matches!(kind, "field_declaration"|"package_declaration")`,
   and fix the walk loop to keep climbing past a matched-but-nameless container
   instead of returning `None`. (Reads `is_symbol_node`; does not modify it.)
2. Add `record_declaration` to `is_refactor_item_kind` (`src/code_nav/mod.rs:742`)
   to make the unreachable Java record branch reachable.
3. Tests for both; `cargo test --lib code_nav` + `cargo clippy`.

Everything else (chunker emission of Rust top-level const/static/macro/type and
Java constructor/record) is DEFERRED to a language/depth-aware emission redesign.

## What I'm asking you to review

1. Is Finding 1 (don't expand synthesis) correct? Find a consumer that would
   justify per-language synthesis if one exists.
2. Is the `is_symbol_node` addition (Finding 2) safe — name resolution for
   macro_definition, no cross-grammar kind collision, equivalence preserved?
3. Is shape (A) for Finding 3 correct, especially the `impl_item` preservation
   and the `source_file`/`package_declaration` exclusion? Any other container
   kinds in `is_symbol_node` that must be excluded from "containing symbol"
   (e.g. `field_declaration`)?
4. Anything that breaks the indexed-vs-live equivalence invariant.
5. Scope: is this the right "depth" increment, or am I missing a cheaper/safer
   higher-value change in this surface?

Verdict format: `APPROVE` / `REVISE` with a numbered must-fix list.

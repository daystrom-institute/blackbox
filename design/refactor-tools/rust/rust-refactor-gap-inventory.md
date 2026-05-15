---
title: "Rust Refactor Gap Inventory - remaining plan kinds and toolbelt additions"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - rust
date: 2026-05-15
status: "design proposal, gap inventory (no implementation phasing)"
brief: "Current Rust refactor gap inventory for remaining plan kinds and toolbelt additions."
---

# Rust Refactor Gap Inventory — remaining plan kinds and toolbelt additions

Related: `refactor-rust-expansion.md`,
`rust-refactor-atoms-batch2.md`,
`refactor-rust-v2-invariants.md`,
`sm-refactor-rust`

## Problem

Three rounds of Rust refactor expansion (G1–G9 from the expansion doc, batch1
atoms, batch2 atoms) filled the high-leverage plan kinds. But the batch2 design
explicitly deferred ~12 atoms because they are blocked on missing plan kinds or
toolbelt additions. The gap notes that were supposed to track these blockers
were either filed under the wrong surface (`packet_ast` → reclassified to
`refactor_primitive` toolbelt gaps), referenced by IDs that don't resolve in the
current notes store, or never created.

This doc is the single inventory. Every Rust refactor gap — plan kind, toolbelt
addition, or compound-run macro — that is known but unimplemented goes here.
New gap notes will be filed from this doc; stale notes will be resolved.

## Non-Goals

- Renegotiating the semantic tiers (`syntax_only` / `indexed_hints` /
  `lsp_verified`) — settled in refactor-rust-expansion.
- Redesigning already-shipped plan kinds.
- Atom manifests themselves — each gap below maps to one or more atoms, but
  atom design is a follow-up step after the plan kind exists.
- Open design questions from the expansion doc (lifetime propagation policy,
  repair loop depth, brofile shipping) — those remain tracked in that doc.
- Catch-all "AI could do it manually" gaps — if a capable agent can already
  compose existing primitives to achieve the goal, it doesn't go here.

## Gap Inventory

Each entry names the missing plan kind(s), the semantic tier, the atom(s) it
unblocks, the gap note ID if one exists, and the nearest existing primitive to
extend.

### G10. `rust_find_references` — project-wide semantic usage queries

**Semantic tier:** `lsp_verified`

**What:** Call `textDocument/references` through rust-analyzer for a named
symbol. Returns per-file usage locations with enough context for caller
rewrites. The existing `rust_ra_classify_callbacks` resolves callees via
`textDocument/definition` only (one impl at a time, call-site → declaration;
the expansion doc spec'd both `references` and `definition` but the
implementation at `rust_ra_classify_callbacks.rs:95` uses only
`GotoDefinition`). `rust_find_references` goes the other direction: given
a declaration, find every call site project-wide via
`textDocument/references`. Trait-object dispatch, blanket impls, re-exports,
and type aliases are resolved by RA — the plan kind just passes through the
LSP response.

**Blocks:** `rust-find-usages` atom, `rust-extract-with-caller-rewrite`
compound operations.

**Nearest existing:** `rust_ra_classify_callbacks` (reverse edge, one-impl
scope). The plan-kind shape would be:
```
bbox_refactor_plan(
    kind="rust_find_references",
    source="src/main.rs",
    item_names=["BlackboxServer"],
    project_dir="/abs/root"
)
```
Return `references: [{file, line, column, context}]` — no FileEdits.

**Gap note:** `note-ca7d7b7d` (filed 2026-05-13, unresolved).

### G11. `rust_ra_extract_function` — borrow-aware function extraction

**Semantic tier:** `lsp_verified`

**What:** Invoke rust-analyzer's `experimental/extractFunction` assist for a
selected region. RA classifies captures (shared borrow, mutable borrow, move,
clone) and infers return type for `?` and `return`. The existing
`extract_rust_function_region` is conservative (tree-sitter only, refuses
`return`/`break`/`continue`/`?`). An LSP-backed kind delegates to RA for the
borrow-checker-aware path.

**Blocks:** `rust-extract-function` atom. This is a high-frequency refactoring
operation that currently requires manual agent-driven extraction with
`cargo check` repair loops.

**Nearest existing:** `extract_rust_function_region` (syntax_only, conservative).
The plan-kind shape would be:
```
bbox_refactor_plan(
    kind="rust_ra_extract_function",
    source="src/foo.rs",
    old_text="<selection>",
    item_names=["new_fn_name"],
    project_dir="/abs/root"
)
```
RA owns the parameter classification and return-type inference. If RA's assist
doesn't fire (e.g. selection spans multiple functions), the plan fails with
`error.lsp_unavailable` rather than downgrading.

**Gap note:** `note-24bde1d7` (filed 2026-05-13, unresolved).

### G12. `rust_derive_audit` — type-aware derive safety analysis

**Semantic tier:** `indexed_hints`

**What:** For a named struct/enum, scan all fields, determine which standard
derives (`Clone`, `Debug`, `PartialEq`, `Eq`, `Hash`, `Default`, `Copy`) are
safe to add, and report existing hand-rolled impls that duplicate a derivable
trait. Safety rules are structural: all fields are `Clone` → safe to
`#[derive(Clone)]`; all fields are `PartialEq` → safe; `Copy` additionally
requires no `Drop` impl and no owned non-`Copy` fields; `Hash` requires all
fields are `Hash`; etc.

Tree-sitter gives field types (name strings). The syntactic type index can
resolve project-local types transitively (a struct field of type `Foo` where
`Foo` derives `Clone` → the field is `Clone`). std and external crate types
require a precomputed whitelist (`String`, `Vec<T>`, `HashMap<K,V>`, etc.).
Unknown types are conservatively treated as not satisfying the trait.

**Blocks:** `rust-audit-derivable` atom (analysis-only),
`rust-derive-from-fields` atom (audit + apply: add derives, delete hand-rolled
impls).

**Nearest existing:** Nothing directly. The Java side has `lombokify_java_class`
which collapses hand-rolled boilerplate to `@Data`/`@Value` — this is the Rust
analogue. Plan-kind shape:
```
bbox_refactor_plan(
    kind="rust_derive_audit",
    source="src/foo.rs",
    item_names=["MyStruct"],
    toml_entries={"derives": ["Clone", "Debug", "PartialEq"]}
)
```
Return `{safe_derives, unsafe_derives_with_reasons, deletable_handrolled_impls}`.

**Gap note:** `note-1fae72a5` (filed 2026-05-13, unresolved).

### G13. `rust_clippy_fix_round` — clippy diagnostic repair

**Semantic tier:** `syntax_only` (edit proposals from structured diagnostics)

**What:** Classify `cargo clippy --message-format=json` output into actionable
edit proposals, analogous to `rust_compile_fix_round` for rustc errors.
Clippy lints with machine-applicable suggestions (`MachineApplicable`) become
`replace_text` proposals; others are reported as warnings. Use only inside
`bbox_refactor_run` with `on_failure="continue_for_repair"`.

**Blocks:** `rust-auto-lint-fix` atom, post-extraction cleanup passes.

**Nearest existing:** `rust_compile_fix_round` (same shape, rustc JSON input).
The plan kind would be nearly identical — parse JSON, classify by lint level
and applicability, emit proposals. The main delta is the diagnostic source
and the classification table (clippy lint codes vs rustc error codes).

**Gap note:** `note-ba562354` (filed 2026-05-13, unresolved).

### G14. `rust_restructure_module_tree` — directory-level module moves with super:: rebasing

**Semantic tier:** `syntax_only` + local indexed hints for visibility

**What:** Move a module directory (e.g. `src/refactor/` → `crate/refactor/`)
and rewrite all `super::` chains to the correct depth at each level.
Updates `mod` declarations across the tree, patches `Cargo.toml` workspace
membership, and handles `use crate::` paths that cross the moving boundary.

The core complexity is `super::` rebasing: a file at depth 2 under the old
root (`src/refactor/rust/plan.rs`) uses `super::super::common` to reach
siblings. After moving to `crate/refactor/rust/plan.rs`, the same reference
might be `super::super::super::common` (if `common` stayed behind) or stay the
same (if `common` moved with it). The plan kind needs to know which files
moved and which stayed.

**Blocks:** `rust-promote-module-to-crate` atom,
`rust-restructure-crate` atom.

**Nearest existing:** `move_rust_items_with_callers` (single-file move with
caller rewrites), `move_file` (single-file move, no content rewrite). The
directory-tree shape is a compound operation spanning many files.

**Gap note:** `note-f16cee20` (filed 2026-05-13, unresolved).

### G15. `rust_add_cfg_attribute` — feature-gate insertion

**Semantic tier:** `syntax_only`

**What:** Add `#[cfg(feature = "foo")]` to selected items and
`cfg_if::cfg_if!` blocks for grouped gates. When inserting a feature gate on an
item that already has attributes, the cfg attribute is inserted after existing
`#[derive(...)]` and before doc comments (per rustfmt convention). A companion operation to add the feature flag to `Cargo.toml` `[features]`
is satisfied by an `ensure_toml_table` call — no separate plan kind needed.

**Blocks:** `rust-add-feature-gate` atom,
`rust-conditional-compile` atom.

**Nearest existing:** No existing attribute-insertion plan kind. Tree-sitter
can locate attribute positions structurally. Plan-kind shape:
```
bbox_refactor_plan(
    kind="rust_add_cfg_attribute",
    source="src/foo.rs",
    item_names=["gated_function"],
    toml_entries={"feature": "my-feature", "predicate": "feature = \"my-feature\""},
    project_dir="/abs/root"
)
```

**Gap note:** `note-4e4e7382` (filed 2026-05-13, unresolved).

### G16. `rust_wrap_return_in_result` — return-type wrapping

**Semantic tier:** `indexed_hints`

**What:** For a function returning `T`, wrap every return site as `Ok(expr)`
and change the return type to `Result<T, E>`. Propagates `?` on call sites
where the caller is also being wrapped. The `?` propagation chain requires
tracking which functions are in the "wrap set" — call sites to functions in the
set get `?` appended; others get `Ok(...)` wrapping at the call site.

**Blocks:** `rust-wrap-in-result` atom. Common pattern when threading an error
type through a call chain that currently panics or unwraps.

**Nearest existing:** Nothing directly. This is more complex than a simple
text replace because of the `?` propagation across the call boundary.

**Gap note:** `note-f8dda062` (filed 2026-05-13, unresolved).

### G17. `rust_inline_module` — reverse of extract-to-submodule

**Semantic tier:** `syntax_only`

**What:** Take a submodule file (`src/foo/bar.rs`) and inline it as
`mod bar { ... }` into the parent (`src/foo.rs`). Deletes the submodule file
and the `mod bar;` declaration, replacing it with the inline body. Preserves
inner attributes and doc comments. The reverse of
`inline_mod_to_file_submodule`.

**Blocks:** `rust-inline-module` atom. Useful when a submodule proves too
small to justify a separate file.

**Nearest existing:** `inline_mod_to_file_submodule` (the forward direction).
The reverse has different edge cases (what to do with `use` declarations in the
child — keep or merge into parent's use block).

**Gap note:** `note-9d2b4184` (filed 2026-05-13, unresolved).

### G18. `rust_test_attribution` — test→production function mapping

**Semantic tier:** `indexed_hints`

**What:** For each public/private function in a source file, find the
`#[test]` functions that exercise it. Uses syntactic call-graph analysis:
a test function calls production function `foo` → attributed to `foo`.
Does not resolve through re-exports or trait dispatch. The output is a
mapping `{fn_name: [test_fn_names]}` that lets extraction operations move
tests alongside their production code.

**Blocks:** `rust-extract-with-tests` atom — when extracting a function to a
new module, its tests should move with it automatically. Currently requires
manual grep.

**Nearest existing:** `rust_top_level_dependency_analysis` (call graph for
top-level items, analysis-only). `rust_test_attribution` is a narrower,
test-focused variant.

**Gap note:** `note-1733147c` (filed 2026-05-13, unresolved; `note-a6ee6d39`
was the original `packet_ast` filing, now addressed).

### G19. `rust_split_trait` — supertrait extraction

**Semantic tier:** `indexed_hints`

**What:** Given an existing trait with N methods, split it into a supertrait
with M methods and a subtrait that extends it with the remaining N-M methods.
Rewrites impl blocks to implement both traits. Reports default-method conflicts
and where-bounds that reference the trait being split.

**Blocks:** `rust-split-trait` atom. Useful when a trait grows too large and a
subset of methods form a coherent abstraction.

**Nearest existing:** `extract_rust_trait` (extract trait from inherent impl).
The split direction is different: starting from a trait, not an impl.

**Gap note:** `note-9a79027b` (filed 2026-05-15).

### G20. `rust_generate_derives` — Deref/AsRef boilerplate for newtypes

**Semantic tier:** `syntax_only`

**What:** For a tuple-struct newtype (`struct Foo(Bar)`), generate
`impl Deref for Foo { type Target = Bar; ... }` and `impl AsRef<Bar> for Foo`
with forwarding bodies. Optionally generate `From<Bar> for Foo` and
`Into<Foo> for Bar` if requested.

**Blocks:** `rust-newtype-wrap` atom.

**Nearest existing:** No structural equivalent. This is boilerplate generation
from a known template — tree-sitter validates the struct shape; the plan emits
known-good impl blocks.

**Gap note:** `note-25af3e2c` (filed 2026-05-15).

### G21. `rust_match_to_enum` — match-arm extraction into enum variants

**Semantic tier:** `indexed_hints`

**What:** Given a match expression whose arms produce structurally similar
code modulo identifiers, generate an enum with one variant per arm, extract
per-arm logic into a `From<Enum> for Output` impl, and replace the match
with `enum_value.into()`. The companion `rust_generate_from_impl` mechanizes
the per-arm extraction.

**Blocks:** `rust-enum-from-match` atom. The motivating case is provider
dispatch: each arm constructs different CLI args for a different provider,
and extracting to an enum + From impl splits the dispatch table cleanly.

**Nearest existing:** `rust_match_arm_to_strategy` (RX-P1, implemented at
`src/refactor/rust_match_strategy.rs:44`) is the complement direction: it
takes an EXISTING enum and generates per-variant strategy modules. G21 goes
the opposite way — match expression on literals/strings → new enum + From impl.
The analysis pass needs to detect structural similarity across match arms
(same function calls, different string/numeric literals). The expansion doc's
open question 4 asked about auto-detection of driver families; RX-P1 resolved
this by accepting explicit `driver_share_groups` input instead of
auto-detection. G21 would similarly accept explicit arm groupings for v1.

**Gap note:** `note-e7493da7` (filed 2026-05-15).

### G22. `rust_migrate_mods_to_lib` — atom wrapping for the existing macro

**Semantic tier:** `syntax_only` (the underlying macro) + atom inputs

**What:** `migrate_rust_mods_to_lib` already exists as a
`bbox_refactor_run` macro expansion. The gap is that no atom wraps it
for autonomous agent dispatch. The atom needs input schema (which modules,
which bin roots, visibility), grounding steps, and post-flight validation.

**Blocks:** `rust-migrate-mods-to-lib` atom.

**Nearest existing:** `migrate_rust_mods_to_lib` (bbox_refactor_run macro,
implemented). The atom is a thin wrapper — the work is in defining inputs
and protocol, not in a new plan kind.

**Gap note:** `note-677e0430` (filed 2026-05-13, unresolved). Also
`note-c699da56` (macro root-alias bug: the underlying macro generates invalid
`use mod <module>;` syntax when repairing unqualified call sites after a
bin→lib move — needs fixing before the atom wrapper is fully useful).

### G23. `rust_doc_harden` — doc-comment-aware plan kind

**Semantic tier:** `syntax_only`

**What:** A plan kind or status extension that inventories doc comments on
public items, detects missing/trivial docs, and can insert stub doc comments.
Tree-sitter parses doc comments (`///`, `//!`, `/** */`, `#[doc = "..."]`)
as structured trivia. The batch2 design deferred the `rust-doc-harden` atom
because `bbox_refactor_status(include_attributes=true)` returns `#[...]`
attributes but not doc comments, and prompt-only slicing is too brittle for
automatic stub insertion.

**Blocks:** `rust-doc-harden` atom (audit + apply).

**Nearest existing:** `rust_public_api_guard` (analysis-only, scores public
API changes). A doc-hardening primitive would be analysis with optional
stub insertion.

**Gap note:** `note-ba99bddd` (filed 2026-05-15).

## Prioritization

Grouped by impact × implementation complexity:

**Tier 1 — unblock high-frequency atoms, moderate implementation cost:**
- G11 `rust_ra_extract_function` — LSP pass-through, RA does the work
- G10 `rust_find_references` — LSP pass-through, RA does the work
- G13 `rust_clippy_fix_round` — clone `rust_compile_fix_round`, swap diagnostic source
- G12 `rust_derive_audit` — structural analysis with known safety rules
- G18 `rust_test_attribution` — filtered variant of existing `rust_top_level_dependency_analysis`

**Tier 2 — new plan-kind machinery, higher implementation cost:**
- G15 `rust_add_cfg_attribute` — new tree-sitter attribute manipulation
- G16 `rust_wrap_return_in_result` — cross-function ? propagation

**Tier 3 — complex analysis or narrow use case:**
- G14 `rust_restructure_module_tree` — multi-file super:: rebasing (iceberg; see OQ5)
- G17 `rust_inline_module` — reverse of existing, moderate complexity
- G19 `rust_split_trait` — trait-level refactoring
- G20 `rust_generate_derives` — boilerplate generation (template-driven)
- G21 `rust_match_to_enum` — structural similarity detection
- G23 `rust_doc_harden` — doc-comment awareness in status + plan

**Tier 4 — atom wrapper, no new plan kind:**
- G22 `rust_migrate_mods_to_lib` — atom wrapping for existing macro

## Open Design Questions

1. **LSP-backed kinds require warm RA session.** G10 and G11 fail closed on
   `error.lsp_unavailable`. The `warm-rust-analyzer-session` worktree
   suggests RA session management is already on the radar. Whether the daemon
   should eagerly warm RA sessions for registered Rust projects or leave it to
   the caller is open.

2. **Index breadth for `indexed_hints` plan kinds.** G12 (derive audit) and
   G18 (test attribution) both consult the project-local type index. Whether
   the index should include workspace crates or stay project-local is the
   same question as expansion doc open question 6. Default to project-local
   for v1; workspace scope adds cost but higher recall.

3. **G16 `rust_wrap_return_in_result` scope.** Should the wrapping be
   call-chain-aware (follow every caller up the stack) or single-function?
   Call-chain wrapping is more useful but requires the cross-function analysis
   that G10 `rust_find_references` would provide. Ship single-function first
   and compose with G10 for chain wrapping.

4. **G21 match-arm similarity heuristic.** The expansion doc's open question 4
   asked about auto-detection of driver families for `rust_match_arm_to_strategy`.
   RX-P1 resolved this: the plan kind ships with explicit `driver_share_groups`
   input (operator-supplied variant groups), and auto-detection is deferred.
   G21 `rust_match_to_enum` (the reverse direction — match expression → new enum)
   should follow the same pattern: accept explicit arm groupings for v1.

5. **G14 `rust_restructure_module_tree` is an iceberg.** Directory tree moves
   touch `mod` declarations, `use` paths, `Cargo.toml`, `super::` chains,
   `crate::` paths, and possibly `#[path]` attributes. A full solution is a
   significant engineering effort. A v1 might support only the simple case
   (one directory, no `#[path]` attributes, no workspace boundary crossing)
   and refuse otherwise.

6. **Gap note hygiene (resolved).** The original 2026-05-13 `plan_kind` gap notes
   for G10–G18 and G22 all exist and are unresolved — the earlier "not found"
   results were a query artifact (text-body matching vs ID lookup). The
   `packet_ast` notes that were superseded by `plan_kind` notes (`note-c270ff36`,
   `note-7f6828db`, `note-fdd2efa6`, `note-3e0d0972`, `note-a6ee6d39`) are all
   addressed. Fresh `refactor_primitive` notes were filed 2026-05-15 for G19–G21
   and G23 (no originals existed). Domain string is inconsistent across vintages
   (`domain: "refactor-rust"` for 2026-05-13 notes vs `domain: "rust-refactor"`
   for 2026-05-15 notes); prefer `rust-refactor` going forward. If new gaps are
   discovered, file under `gap_kind: "refactor_primitive"` or `kind: "plan_kind"`,
   `domain: "rust-refactor"`.

## Rejected

- **`rust_wrap_unsafe` plan kind.** Wrapping unsafe blocks is a
  single-attribute or keyword insertion; `replace_text` handles it.
- **`rust_sort_impl_items` plan kind.** `rust_organize_imports` already
  cleans imports; impl-method ordering is a style question that `rustfmt`
  doesn't enforce. Not worth a plan kind.
- **`rust_expand_macro` plan kind.** Proc-macro expansion requires
  compilation; this is a `cargo expand` command step, not a refactor
  plan kind.
- **`rust_migrate_to_edition` plan kind.** Edition migration is `cargo fix
  --edition` territory — a toolchain command, not a refactor primitive.

# Rust Refactor Tooling Gaps

This note records what the current `bbox_refactor_*` Rust tooling can already
handle in this repo, and which tool gaps would turn the remaining god-file
cleanup from hand surgery into guarded mechanical work.

## Can Do Now

These shapes are already supported by existing plan kinds and were validated by
dry-run planning during scouting:

- Move inline test modules into sibling files with `inline_mod_to_file_submodule`.
  Confirmed targets:
  - `src/packets/mod.rs` `mod tests` -> `src/packets/tests.rs`
  - `src/workflow/engine.rs` `mod tests` -> `src/workflow/engine/tests.rs`
- Extract cohesive top-level helper clusters into child modules with
  `extract_rust_items_to_submodule`.
  Confirmed target:
  - `src/refactor/java.rs` import/type-index helpers -> `src/refactor/java/imports.rs`
- Move selected inherent impl methods into sibling module files with
  `extract_rust_impl_methods`.
  Confirmed target:
  - `WorkflowRunner` fanout methods -> `src/workflow/engine/fanout.rs`
- Compose basic support edits with existing primitives:
  - `add_rust_mod_decl`
  - `add_rust_use_decl`
  - `rewrite_rust_item_visibility`
  - `rewrite_rust_field_visibility`
  - `rust_organize_imports`
  - `rust_compile_fix_round`

The practical current ceiling is not text movement. It is reliably choosing
cohesive groups, applying the companion module/visibility/import edits, and
using `cargo check` feedback to repair the result without manual drift.

## Current High-Value Targets

1. `src/packets/mod.rs`
   - Extract inline tests first. This cuts roughly 3.7k lines from the module
     with low semantic risk.

2. `src/refactor/java.rs`
   - Extract helper domains in small batches:
     - `imports`
     - `members`
     - `accessors`
     - `callbacks`
     - `visibility`
   - This is the best production-file target for current tooling because much
     of the file is top-level helper functions and structs.

3. `src/workflow/engine.rs`
   - Split `WorkflowRunner` by node family:
     - `fanout`
     - `wait`
     - `subworkflow`
     - `ensemble`
     - `gates`
     - `hooks`
   - Current tools can move the methods, but a compound plan should own the
     module declaration, visibility widening, import organization, cargo check,
     and compile-fix loop.

4. `src/refactor/mod.rs`
   - Split cross-cutting refactor infrastructure:
     - `types`
     - `dispatcher`
     - `apply`
     - `run`
     - `syntax`
     - `toml`
     - `fs_txn`
   - This should be phased after the easier cuts because many language-specific
     modules depend on names currently flattened through this file.

5. `src/main.rs`
   - Shrink `main()` and server bootstrap into runtime/server modules.
   - Current tools are weak here because the largest problem is intra-function
     extraction, not top-level item movement.

## Tool Gaps

### G1: Rust Dependency-Cluster Analysis for Top-Level Items

Current state: `extract_rust_items_to_submodule` moves named items, but the
operator must choose the group.

Wanted: analysis-only plan that reports a dependency graph for top-level Rust
items in one file:

- item -> item calls
- item -> type references
- item -> module/global/static/const references
- external references from the rest of the crate
- suggested cohesive clusters
- warnings for macro-heavy or unresolved edges

This would make `src/refactor/java.rs` and `src/refactor/mod.rs` splits much
less guessy.

### G2: Compound `split_rust_impl_methods_to_submodule`

Current state: `extract_rust_impl_methods` moves methods, but companion edits
are separate.

Wanted: one transactional plan/run shape that:

- moves selected impl methods into a child module file
- adds the child `mod` declaration
- widens moved methods to `pub(super)` or requested visibility
- adds target prelude
- organizes imports
- runs `cargo check --message-format=json`
- feeds diagnostics through `rust_compile_fix_round`
- runs final `cargo check` and optional targeted tests

This is the missing primitive for splitting `WorkflowRunner` safely.

### G3: Intra-Function Extraction

Current state: no guarded way to extract blocks from a large function into new
helper functions.

Wanted: `extract_rust_function_region` backed by rust-analyzer where possible:

- identify selected statement range or AST node
- infer parameters from captured locals
- infer return type / early-return behavior
- create helper function
- replace original block with call
- reject complex borrow/label/control-flow cases

This is the main blocker for shrinking `src/main.rs`'s large `main()`.

### G4: Wildcard Import Minimizer

Current state: many modules rely on `use super::*` or `use crate::*`; moving
code often preserves this broad coupling.

Wanted: `rust_minimize_imports`:

- replace wildcard imports with explicit imports needed by the file
- work after extraction to make new module boundaries real
- preserve intentional prelude imports behind an allowlist
- run rust-analyzer organize-imports afterward

This would improve idiomatic Rust module hygiene after mechanical splits.

### G5: Bin-to-Lib Migration Support

Current state: the crate has a `[lib]` shell, but most modules are still owned
by `src/main.rs`.

Wanted: compound migration that moves selected `mod` declarations from a binary
root to `lib.rs` and repairs paths across binaries:

- add `pub mod`/`pub(crate) mod` in `lib.rs`
- remove duplicate binary `mod` declarations
- rewrite `crate::foo` / `blackbox::foo` references as needed
- preserve binary-private modules
- validate every binary target

This would make `blackboxd`, `bro`, `bro-irc`, and `bro-slack` thinner and more
idiomatic.

### G6: Stringly Field to Typed Enum Migration

Current state: many MCP/refactor params still use free-form string fields for
`kind`, `action`, `scope`, `provider`, and status-like values.

Wanted: semantic migration helper:

- introduce enum with serde rename/alias behavior
- rewrite matches from `as_str()` strings to enum variants
- update schemas/tests
- preserve external JSON compatibility

This is not a god-file split, but it is one of the highest-value idiomatic Rust
upgrades.

### G7: Test-Module Extraction With Fixture Repair

Current state: inline test module extraction works for simple cases.

Wanted: test-focused compound plan:

- extract `#[cfg(test)] mod tests`
- add `mod tests;`
- keep test-only imports local
- optionally split very large test files by test-name clusters
- rewrite fixture path assumptions if the file moves deeper

This would avoid turning one god file into one giant `tests.rs` dumping ground.

## Suggested Priority

1. Use current tooling on low-risk bulk: inline tests and obvious top-level
   helper clusters.
2. Build G2 before attempting broad `WorkflowRunner` partitioning.
3. Build G1 before continuing deep `refactor/mod.rs` surgery.
4. Build G3 before tackling `main()` seriously.
5. Use G4 and G5 as cleanup multipliers after the first successful splits.

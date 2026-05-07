# Refactor Surface Benchmark: `design/restructure.md`

Date: 2026-05-07
Status: working benchmark

## Purpose

`design/restructure.md` is the exemplar for the `bbox_refactor_*` MCP surface.
The goal is not to manually perform that restructure in this repository. The
goal is to make the MCP surface general enough that an agent could honestly do
all or most of that restructure through reusable refactor operations.

Victory condition: for every restructure step, either:

- there is a generic MCP operation that can perform it with reviewable plans,
  hash checks, rollback, syntax validation, and compiler/LSP gates, or
- the missing capability is explicitly named and implemented before we claim
  the restructure is mechanized.

No benchmark-only shortcuts. If a tool would only work for Blackbox's current
file names or tool prefixes, it does not count.

## Capability Matrix

| Restructure Step | Needed Generic MCP Capability | Current Coverage | Gap |
|---|---|---|---|
| Add `[lib]` target to `Cargo.toml` | TOML table insertion/update with idempotent key checks | covered with `ensure_toml_table` | none for top-level table/string entries |
| Create `src/lib.rs` from `mod` declarations in `main.rs` | extract/copy module declarations; transform copied declarations to `pub mod`; create file | covered for declaration copy/rewrite with `copy_rust_mod_decls`, `rewrite_rust_mod_visibility`, and `write_file` | none for declaration-driven lib bootstrap |
| Delete `mod foo;` from `main.rs` after lib reparent | syntactic deletion of selected top-level `mod_item`s | covered with `delete_rust_items`; can participate in `bbox_refactor_run` | none for top-level declarations |
| Rewrite binary crate references from `crate::foo` to `blackbox::foo` where needed | explicit path-prefix rewrite plus compiler validation | assisted with `replace_text(replace_all=true)` only after grounding exact intended text | not symbolic; use `rust_lsp_rename` for real symbol rename |
| Move inline `SharedState`, `BlackboxServer`, impls, routes, tests to `server/mod.rs` | top-level item extraction; impl extraction; route/helper extraction; test block extraction; exact file rewrite fallback | covered with extraction primitives plus `write_file` after grounding | no automatic dependency closure; plan must name moved items explicitly |
| Shrink `main.rs` to bootstrap only | create replacement file from selected retained functions/imports | covered with `write_file` after grounding and command validation | none for checkpointed rewrite |
| Split one tool domain per `tools/<domain>.rs` | extract selected `#[tool]` impl methods; create router helper; add module decl; wire router sum | covered with `extract_rust_impl_methods`, `add_rust_mod_decl`, `add_rust_router_to_sum`, `bbox_refactor_run` | grouping by attribute/name prefix remains agent-grounded, not automatic |
| Move parameter structs with tool handlers | extract top-level structs near selected methods by type usage | covered with `extract_rust_items` by exact grounded names | automatic symbol dependency discovery remains future |
| Update `BlackboxServer::new` router sum | insert router call into `tool_router:` field initializer | covered; can participate in `bbox_refactor_run` | none for existing router-sum shape |
| Move file-local helper functions with a domain | dependency closure over selected handlers | covered with `extract_rust_items` by exact grounded helper names | automatic call graph remains future |
| Move per-domain tests next to code | test discovery and relocation | assisted with `write_file` after grounding, plus command validation | automatic test-to-symbol association remains future |
| Convert `src/packets.rs` to `src/packets/mod.rs` | git/file move; update module layout; preserve module identity | covered for file move with `move_file` | module-layout declaration follow-up still manual/compound |
| Split `packets` by layer (`ast`, `compile`, `apply`, etc.) | extract AST/types/functions/tests by dependency layers | covered with `extract_rust_items`, `add_rust_mod_decl`, `add_rust_use_decl`, and exact rewrite plans after grounding | automatic dependency closure/import repair remains future |
| Add `pub mod ast;` etc. inside `packets/mod.rs` | module declaration insertion | covered with visibility option | compound transaction integration |
| Re-export moved symbols for compatibility | add `pub use` statements | covered with `add_rust_use_decl` | semantic choice of re-export path still manual/LSP-assisted |
| Split HTTP routes into route modules | extract functions/structs + update route builder references | covered with `extract_rust_items`, `write_file`, and command validation | automatic reference discovery remains future; symbolic renames use `rust_lsp_rename` |
| Move free functions referenced from siblings | extract functions and update paths or add re-exports | covered with extraction plus `add_rust_use_decl` and `rust_lsp_rename` where symbols are renamed | automatic reference discovery remains future |
| Run format/check/test after every step | command validation in transaction | covered with `bbox_refactor_run` command steps | structured diagnostic parsing remains future |
| Rollback across multi-step refactor | transaction across several primitive plans + validations | covered: `bbox_refactor_run` snapshots primitive-plan file writes and rolls back on required plan or command failure | temp-worktree validated diff mode remains future |
| Optimize imports after moves | semantic import organize/prune | covered per file with `rust_organize_imports` backed by rust-analyzer `source.organizeImports`; explicit missing imports still use `add_rust_use_decl` plus compiler validation | fully automatic missing-import repair remains future |
| Symbolic rename/reference rewrite | workspace-safe semantic rename | covered with `rust_lsp_rename` backed by rust-analyzer `textDocument/rename` | none for rename-capable Rust symbols |

## Generic Operations Required

### Structural Rust Operations

- `add_rust_mod_decl`
  - add `mod name;`, `pub mod name;`, or `pub(crate) mod name;`
- `add_rust_use_decl`
  - insert `use path;` or `pub use path;`
  - place after module declarations and before other items
  - idempotent duplicate detection
- `delete_rust_items`
  - delete top-level items or impl methods by exact syntactic identity
  - require explicit `item_names`; `item_kinds` can narrow but not select alone
  - same parse/hash/dirty checks as extraction
  - implemented for top-level items and impl methods; nested module items remain
    future work
- `copy_rust_mod_decls`
  - copy selected source `mod name;` declarations into another Rust file
  - optionally rewrite copied declaration visibility
  - rejects inline `mod name { ... }` modules
- `rewrite_rust_mod_visibility`
  - `mod foo;` -> `pub mod foo;`, `pub(crate) mod foo;`, or private
  - applies to existing declarations in place
- `extract_rust_nested_module_items`
  - move items out of `#[cfg(test)] mod tests { ... }`
  - move nested support modules without flattening incorrectly

### Semantic Rust Operations

- `rust_symbol_dependency_closure`
  - given selected functions/methods/types, find local helper functions,
    parameter structs, response structs, constants, and tests likely required
    for a coherent move
  - tree-sitter can seed candidates; rust-analyzer confirms references
- `rust_lsp_rename`
  - backed by rust-analyzer `textDocument/rename`
  - converts `WorkspaceEdit` to `FileEdit`
- `rust_import_repair`
  - apply rust-analyzer missing-import code actions
  - parse and compiler validate
- `rust_import_prune`
  - implemented as `rust_organize_imports` for rust-analyzer
    `source.organizeImports`

### Cross-Language/Generic Operations

- `ensure_toml_table`
  - structured TOML edits for top-level tables such as `[lib]`
  - preserves unrelated manifest content and validates TOML after planning
- `move_file`
  - move any file to a missing target path
  - records old/new path, creates parent dirs, preserves source bytes, rejects
    existing targets, validates supported source syntax at the destination, and
    rolls back write/remove failures
- `replace_text`
  - exact string replacement in any UTF-8 file
  - defaults to exactly-one match; `replace_all=true` is explicit
  - validates supported source files after rewriting
  - does not count as semantic rename or import repair
- `write_file`
  - replace or create an entire UTF-8 file under hash/path/dirty checks
  - validates supported source files after rewriting
- `compound_run`
  - compose primitive plans and rollback
  - V1 implemented as `bbox_refactor_run` for primitive plans, with live
    sequential planning and rollback across primitive-plan file writes and
    required command failures
- `command_validation`
  - implemented as `{"op":"command","command":"...","args":[...]}` steps in
    `bbox_refactor_run`
  - language memories should provide profile details for cargo, npm, dotnet,
    go, pytest, etc.
  - commands are validation-only unless `touches` declares paths they may
    mutate; declared touches are snapshotted and rolled back with the run
- `diagnostic_parse`
  - parse cargo/rustc diagnostics into structured file/range/code messages

## Restructure-Driven Implementation Order

1. **Compound run MVP**
   - compose existing Rust primitive plans
   - live sequential planning against the written result of prior steps
   - rollback across primitive-plan touched files
   - future: generic validation profiles, temp-worktree projection, mutating
     formatter capture, LSP steps

2. **Deletion and declaration batching**
   - `delete_rust_items` for top-level items and impl methods is covered
   - `copy_rust_mod_decls` and `rewrite_rust_mod_visibility` are covered for
     module-declaration reparenting

3. **File move support**
   - `move_file` is covered for missing-target file moves
   - especially `src/packets.rs` -> `src/packets/mod.rs`
   - future: module-layout update helper and temp-worktree validated diff mode

4. **Structured TOML and exact rewrite edits**
   - `ensure_toml_table` covers adding `[lib]`
   - `replace_text` covers explicit literal text rewrites only
   - `write_file` covers checkpointed bootstrap-file rewrites

5. **Rust LSP adapter**
   - `rust_lsp_rename` covers binding-aware symbol rename
   - `rust_organize_imports` covers per-file import organization
   - fully automatic missing-import repair remains future; current benchmark can
     still proceed by adding explicit imports and compiler-validating

6. **Dependency closure and test relocation**
   - benchmark plan must ground exact names/ranges before each move
   - automatic helper/test association remains future, but no benchmark step
     requires it to be automatic

## Benchmark Protocol

For each future MCP capability:

1. Add a fixture mini-crate proving the operation generically.
2. Run the operation on a disposable worktree of Blackbox.
3. Require the target project's relevant validation command to pass; for this
   repo's practice branches, that is usually `cargo test --bin blackboxd` or a
   narrower Rust command.
4. Commit the practice branch as evidence.
5. Only then count the restructure step as covered.

The final claim should be phrased in terms of coverage:

- "Mechanized": performed by MCP tools on a practice worktree and validated.
- "Assisted": MCP generated part of the work, but an agent still performed
  manual semantic repair.
- "Missing": not yet exposed as reusable MCP surface.

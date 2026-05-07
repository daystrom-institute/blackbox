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
| Add `[lib]` target to `Cargo.toml` | TOML table insertion/update with idempotent key checks | none | `edit_toml_manifest` or generic structured-config edit |
| Create `src/lib.rs` from `mod` declarations in `main.rs` | extract/copy module declarations; transform copied declarations to `pub mod`; create file | partial: `add_rust_mod_decl` can create new declarations with visibility | batch module-declaration extraction/copy; existing declaration visibility rewrite |
| Delete `mod foo;` from `main.rs` after lib reparent | syntactic deletion of selected top-level `mod_item`s | covered with `delete_rust_items`; can participate in `bbox_refactor_run` | none for top-level declarations |
| Rewrite binary crate references from `crate::foo` to `blackbox::foo` where needed | semantic path rewrite across crate boundary | none | rust-analyzer/workspace reference rewrite plus compiler diagnostics |
| Move inline `SharedState`, `BlackboxServer`, impls, routes, tests to `server/mod.rs` | top-level item extraction; impl extraction; route/helper extraction; test block extraction | partial: top-level + impl method extraction | nested module/test extraction, multi-item grouping, visibility/import repair |
| Shrink `main.rs` to bootstrap only | create replacement file from selected retained functions/imports | none | file rewrite plan from template + import pruning |
| Split one tool domain per `tools/<domain>.rs` | extract selected `#[tool]` impl methods; create router helper; add module decl; wire router sum | strong for current Rust tool pattern | compound run wrapper; grouping by attribute/name prefix |
| Move parameter structs with tool handlers | extract top-level structs near selected methods by type usage | partial: top-level item extraction by exact names | symbol dependency discovery from method signatures |
| Update `BlackboxServer::new` router sum | insert router call into `tool_router:` field initializer | covered; can participate in `bbox_refactor_run` | none for existing router-sum shape |
| Move file-local helper functions with a domain | dependency closure over selected handlers | none | call graph / reference extraction via rust-analyzer |
| Move per-domain tests next to code | test discovery and relocation | none | test-to-symbol association and nested `#[cfg(test)]` module editing |
| Convert `src/packets.rs` to `src/packets/mod.rs` | git/file move; update module layout; preserve module identity | none | transactional file move/rename op |
| Split `packets` by layer (`ast`, `compile`, `apply`, etc.) | extract AST/types/functions/tests by dependency layers | partial for top-level items | dependency closure, import repair, test relocation |
| Add `pub mod ast;` etc. inside `packets/mod.rs` | module declaration insertion | covered with visibility option | compound transaction integration |
| Re-export moved symbols for compatibility | add `pub use` statements | covered with `add_rust_use_decl` | semantic choice of re-export path still manual/LSP-assisted |
| Split HTTP routes into route modules | extract functions/structs + update route builder references | partial for top-level functions | reference rewrite, import repair, path update |
| Move free functions referenced from siblings | extract functions and update `crate::...` paths or add re-exports | partial for extraction | LSP find refs/rename; `pub use` insertion |
| Run format/check/test after every step | command validation in transaction | none in generic MCP surface | generic validation/profile surface; language memories can name cargo/npm/dotnet/etc. commands |
| Rollback across multi-step refactor | transaction across several primitive plans + validations | partial: `bbox_refactor_run` snapshots primitive-plan file writes and rolls back on required plan-step failure | temp-worktree validated diff mode; validation-step integration |
| Optimize imports after moves | semantic import organize/prune | none | rust-analyzer organize imports / code actions |
| Symbolic rename/reference rewrite | workspace-safe semantic rename | none | LSP-backed `lsp_rename` |

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
- `move_rust_file`
  - `src/foo.rs` -> `src/foo/mod.rs` or arbitrary file move
  - records old/new path, creates parent dirs, updates rollback
- `rewrite_rust_mod_visibility`
  - `mod foo;` -> `pub mod foo;` or `pub(crate) mod foo;`
  - distinct from `add_rust_mod_decl.visibility`, which only controls newly
    inserted declarations
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
  - backed by rust-analyzer `prepareRename` + `rename`
  - converts `WorkspaceEdit` to `FileEdit`
- `rust_import_repair`
  - apply rust-analyzer missing-import code actions
  - parse and compiler validate
- `rust_import_prune`
  - organize imports or exact unused-import deletions

### Cross-Language/Generic Operations

- `edit_toml_manifest`
  - structured TOML edits for `[lib]`, `[[bin]]`, dependencies, workspace
    members
- `compound_run`
  - compose primitive plans and rollback
  - V1 implemented as `bbox_refactor_run` for primitive plans, with live
    sequential planning and rollback across primitive-plan file writes
  - validation and LSP steps should attach through generic extension points,
    not Rust-specific command handling
- `command_validation`
  - future generic validation/profile surface
  - language memories should provide profile details for cargo, npm, dotnet,
    go, pytest, etc.
  - declared mutating globs remain future work
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
   - copy/batch module declarations from one file to another

3. **File move support**
   - `move_file`
   - especially `src/packets.rs` -> `src/packets/mod.rs`
   - rollback and hash checks for file existence/non-existence

4. **Structured TOML edits**
   - add `[lib]`
   - preserve formatting where practical
   - validate with `cargo metadata` or `cargo check`

5. **Rust LSP adapter**
   - `rust_lsp_rename`
   - find references / go to definition
   - missing import code actions
   - organize imports

6. **Dependency closure and test relocation**
   - discover helper functions and DTO structs used by moved handlers
   - associate tests with symbols via references
   - support nested `#[cfg(test)] mod tests` extraction

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

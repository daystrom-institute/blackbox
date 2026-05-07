# Rust Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or splitting Rust code with
blackbox refactor tools.

## Current Capability

Rust is the first writable backend.

- Inspect: supported with `bbox_refactor_status`.
- Plan: supported with `bbox_refactor_plan(kind="extract_rust_items")`.
- Apply: supported with `bbox_refactor_apply(confirm=true)`.
- Semantic rename: not supported by blackbox yet; use rust-analyzer, compiler
  feedback, or manual edits after inspection.
- Import repair: not automatic; use `cargo fmt`, `cargo check`, `cargo clippy`,
  and rust-analyzer feedback after structural moves.

Tree-sitter language: `rust`.

Writable plan kinds:

- `extract_rust_items`: move named top-level Rust items from one file to another.

Supported top-level item kinds include:

- `mod_item`
- `use_declaration`
- `struct_item`
- `enum_item`
- `trait_item`
- `function_item`
- `impl_item`
- `macro_definition`
- `const_item`
- `static_item`
- `type_item`

## Tool Sequence

1. Inventory the source file:

```text
bbox_refactor_status(
  file="src/path.rs",
  project_dir="/absolute/project/root"
)
```

Copy exact `name` and `kind` values from the returned `items`. For `impl_item`,
the name is the impl header, not a type identifier.

2. Create a dry-run move plan:

```text
bbox_refactor_plan(
  kind="extract_rust_items",
  source="src/path.rs",
  target="src/path/moved.rs",
  item_names=["helper_name"],
  item_kinds=["function_item"],
  project_dir="/absolute/project/root"
)
```

The plan records absolute file paths, original SHA-256 hashes, non-overlapping
byte edits, selected items, leftovers, and tree-sitter validation steps. Review
it before apply.

3. Apply only after review:

```text
bbox_refactor_apply(
  plan=<plan-json>,
  confirm=true
)
```

Apply refuses stale file hashes. It reparses changed supported source files and
writes atomically with rollback on write failure. Apply is scoped to registered
projects and refuses dirty git files by default; use
`allow_dirty_worktree=true` only when you intentionally planned against current
uncommitted edits.

4. Run the Rust toolchain:

```text
cargo fmt
cargo check
cargo test --bin blackboxd
```

Use a narrower test command when the changed package has a clearer local test.

## Safety Rules

- Treat tree-sitter success as syntax validation, not semantic proof.
- Do not use structural moves for macro-heavy code without `cargo check`.
- Moving an item can require module declarations, `pub` visibility changes,
  import cleanup, or call-site path edits. The current refactor tools do not
  do that automatically.
- For symbolic rename, require an LSP-backed or compiler-verified workflow.
- For broad autonomous restructuring, use a durable bro plus reviewer loop; the
  refactor tools supply mechanical edits, not architectural judgment.

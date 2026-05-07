# Rust Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or splitting Rust code with
blackbox refactor tools.

## Current Capability

Rust is the first writable backend.

- Inspect: supported with `bbox_refactor_status`.
- Plan: supported with `bbox_refactor_plan(kind="extract_rust_items")` and
  `bbox_refactor_plan(kind="extract_rust_impl_methods")` and
  `bbox_refactor_plan(kind="add_rust_router_to_sum")` and
  `bbox_refactor_plan(kind="add_rust_mod_decl")`.
- Apply: supported with `bbox_refactor_apply(confirm=true)`.
- Semantic rename: not supported by blackbox yet; use rust-analyzer, compiler
  feedback, or manual edits after inspection.
- Import repair: not automatic; use `cargo fmt`, `cargo check`, `cargo clippy`,
  and rust-analyzer feedback after structural moves.

Tree-sitter language: `rust`.

Writable plan kinds:

- `extract_rust_items`: move named top-level Rust items from one file to another.
- `extract_rust_impl_methods`: move named methods out of one `impl` block into
  another file, preserving method attributes and optionally generating a
  `#[tool_router(router = name)]` wrapper around the moved methods.
- `add_rust_router_to_sum`: append `+ Self::<router_name>()` to a Rust
  `tool_router:` field initializer if that router call is not already present.
- `add_rust_mod_decl`: add `mod <module_name>;` after existing top-level module
  declarations.

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
  project_dir="/absolute/project/root",
  item_kinds=["impl_method"],
  limit=100,
  include_attributes=false
)
```

Copy exact `name` and `kind` values from the returned `items`. For `impl_item`,
the name is the impl header, not a type identifier. Rust status also includes
`impl_method` entries for methods directly inside impl bodies. Use filters on
large files; status returns `total_items`, `matching_items`, `returned_items`,
and `truncated` so agents can tell when to narrow the query.

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

For methods inside a server/tool impl, use the impl-method plan:

```text
bbox_refactor_plan(
  kind="extract_rust_impl_methods",
  source="src/main.rs",
  target="src/tools/search.rs",
  item_names=["bbox_search", "bbox_browse"],
  item_kinds=["impl_method"],
  impl_name="impl BlackboxServer",
  router_name="search_tools",
  router_export_name="router",
  target_prelude="use super::*;",
  project_dir="/absolute/project/root"
)
```

If `router_name` is present, the target wrapper is generated as
`#[tool_router(router = search_tools)] impl BlackboxServer { ... }`. This
mechanizes the syntax move only. If `router_export_name` is also present, the
target file gets a helper such as
`pub(super) fn router() -> ToolRouter<BlackboxServer>` that calls the private
generated associated router from inside the same module. This is needed when the
new router impl lives in a child module.

You still need to wire the generated router into the server constructor, add
module declarations, fix imports/visibility, and run the Rust toolchain.

If the target already has a matching `impl` block with the same `router_name`,
the moved methods are appended inside that existing impl. Otherwise the plan
creates a new wrapper. `target_prelude` is inserted near the top of a non-empty
target file when it is not already present, after any shebang, crate-level inner
attributes, and crate-level inner doc comments.
If the existing target impl has a different router name, the plan creates a
separate sibling router wrapper rather than merging into the wrong router.

After extracting a new tool-domain impl, wire it into the server router sum:

```text
bbox_refactor_plan(
  kind="add_rust_router_to_sum",
  source="src/main.rs",
  router_call="search_tools::router()",
  project_dir="/absolute/project/root"
)
```

Use `router_name="search_tools"` instead when the router impl remains in the
same module as the constructor and `Self::search_tools()` is visible.

Add the module declaration:

```text
bbox_refactor_plan(
  kind="add_rust_mod_decl",
  source="src/main.rs",
  module_name="search_tools",
  project_dir="/absolute/project/root"
)
```

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
uncommitted edits. For disposable practice worktrees, pass
`allow_unregistered_paths=true` to skip project registration while keeping hash,
dirty-file, parse, and atomic-write safeguards.

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
- Plain `//` comments above a method are not treated as owned method trivia
  unless attached to an attribute/doc block. Convert durable method comments to
  rustdoc before moving when the comment must follow the method.
- For symbolic rename, require an LSP-backed or compiler-verified workflow.
- For broad autonomous restructuring, use a durable bro plus reviewer loop; the
  refactor tools supply mechanical edits, not architectural judgment.

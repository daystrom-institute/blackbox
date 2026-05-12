# Rust Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or splitting Rust code with
blackbox refactor tools.

## Current Capability

Rust is the first writable backend. Plan dispatcher exposes the kinds below;
ask `bbox_refactor_plan` (dry-run) before `bbox_refactor_apply(confirm=true)`.

Inspect: `bbox_refactor_status`. Apply: `bbox_refactor_apply(confirm=true)`.
Compound: `bbox_refactor_run(confirm=true)` runs ordered plan + command steps
with rollback across primitive-plan file writes if a later required step fails.

Plan kinds, grouped by intent:

- Syntactic moves / deletes: `extract_rust_items`, `extract_rust_impl_methods`,
  `lift_rust_inherent_to_free`, `extract_rust_trait`, `move_rust_struct_fields`,
  `delete_rust_items`, `move_file`.
- Caller / accessor rewrites: `update_rust_callers`, `migrate_rust_type_usages`,
  `add_rust_delegate_field`, `add_rust_router_to_sum`.
- Module wiring: `add_rust_mod_decl`, `add_rust_use_decl`, `copy_rust_mod_decls`.
- Visibility: `rewrite_rust_mod_visibility`, `rewrite_rust_item_visibility`,
  `rewrite_rust_field_visibility`.
- LSP-backed (rust-analyzer): `rust_lsp_rename` (semantic rename),
  `rust_organize_imports` (per-file `source.organizeImports`),
  `rust_ra_move_item_to_module` (semantic move + cross-file caller rewrite),
  `rust_ra_classify_callbacks` (resolve method callees via goto-definition).
- Analysis only (no FileEdits): `rust_impl_partition_analysis` (impl-method
  graph for split planning), `rust_public_api_guard` (advisory for visibility
  changes touching public API).
- Run-loop integration: `rust_compile_fix_round` (classify a `capture=rustc_json`
  step's diagnostics into use-decl / visibility / replace proposals).
- Generic primitives (language-agnostic, useful in compound runs):
  `replace_text`, `write_file`, `ensure_toml_table`.

Tree-sitter language: `rust`.

Writable plan kinds:

- `extract_rust_items`: move named top-level Rust items from one file to another.
  Optional `target_prelude` is inserted before generated target content when it
  is not already present; use it for child-module imports needed by derives and
  external crates.
- `extract_rust_impl_methods`: move named methods out of one `impl` block into
  another file, preserving method attributes/modifiers such as `async` and
  optionally generating a `#[tool_router(router = name)]` wrapper around the
  moved methods.
- `delete_rust_items`: delete named top-level Rust items or named impl methods
  in place. `item_names` is required; use `item_kinds` only to narrow matches.
  Use `item_kinds=["impl_method"]` plus `impl_name` when method names are
  ambiguous across impl blocks. A single delete plan cannot mix top-level items
  and impl methods.
- `add_rust_router_to_sum`: append `+ Self::<router_name>()` to a Rust
  `tool_router:` field initializer if that router call is not already present.
- `add_rust_mod_decl`: add `mod <module_name>;` after existing top-level module
  declarations. Optional `visibility` supports `pub` and `pub(crate)`.
- `add_rust_use_decl`: add `use <use_path>;`, `pub use <use_path>;`, or
  `pub(crate) use <use_path>;` after existing top-level use declarations.
- `copy_rust_mod_decls`: copy selected source `mod name;` declarations into a
  target Rust file, optionally rewriting their visibility. Use this for
  reparenting declarations into `lib.rs`; inline `mod name { ... }` modules are
  rejected.
- `rewrite_rust_mod_visibility`: rewrite an existing `mod name;`,
  `pub mod name;`, or `pub(crate) mod name;` declaration to requested
  visibility (`private`, `pub`, `pub(crate)`, or `pub(super)`).
- `rewrite_rust_item_visibility`: rewrite selected top-level Rust items or
  selected impl methods to requested visibility (`private`, `pub`,
  `pub(crate)`, or `pub(super)`). Use `item_kinds=["impl_method"]` plus
  `impl_name` when method names are ambiguous across impl blocks. Use this
  after extracting items into child modules when the parent must still call
  helper functions, constructors, or inherent methods. To promote every method
  in one impl block, omit `item_names` and pass
  `item_kinds=["impl_method"]` plus `impl_name`. The rewrite preserves Rust
  qualifiers such as `async`, `unsafe`, or `const` while changing only the
  visibility prefix.
- `rewrite_rust_field_visibility`: rewrite every named field in one or more
  selected Rust structs to requested visibility. Pass `item_names` as the
  struct names, for example `item_names=["SharedState","BlackboxServer"]`,
  with `visibility="pub(crate)"` after moving state structs into child modules
  while existing sibling modules still access their fields.
- `rust_lsp_rename`: rename a Rust symbol through rust-analyzer. Pass
  `item_names=["old_name"]` or `old_text="old_name"` plus
  `new_text="new_name"` and `source` pointing at a file containing the symbol
  declaration. The plan is `semantic_status="lsp_verified"` and can touch
  multiple files.
- `rust_organize_imports`: request rust-analyzer `source.organizeImports` for
  `source` and emit the resulting workspace edit as normal hash-checked edits.
- `rust_ra_move_item_to_module`: requests rust-analyzer's `refactor.move`
  code action at the named item's byte range. **The tool name oversells its
  behavior.** The `target` parameter is title-only — it is NOT sent to RA;
  RA decides the destination itself. In observed practice (rust-analyzer
  1.95) the action does not fire for top-level items moved between sibling
  modules, nor for inline `mod foo { ... }` blocks moved to a file. Expect
  `error.lsp_unavailable: no move-to-module code action found for <name>`
  for most realistic use cases. Until either the tool or the RA assist
  catches up, use `extract_rust_items` (top-level items) or a manual
  body-extract + `mod foo;` declaration rewrite (inline-mod-to-file).
  Accepts `function_item`, `struct_item`, `enum_item`, `trait_item`,
  `type_item`, `const_item`, `static_item`, `mod_item`. Refuses
  `impl_method` (use `extract_rust_impl_methods`).
- `rust_ra_classify_callbacks`: walks call sites of named methods in `source`
  and asks rust-analyzer `textDocument/definition` to resolve where each
  callee is declared. Returns `resolved_callbacks` (one entry per method with
  declaring item/kind and call-site previews) without editing any file. Use
  before an extract to see whether a moved cluster would still resolve.
- `lift_rust_inherent_to_free`: lift named methods out of an inherent `impl`
  block into free functions in another file. Methods that capture state, take
  `&self` / `&mut self`, or otherwise depend on the impl receiver are refused
  with structured `refusal_reasons`; eligible methods move with their bodies
  and the source-side impl method is deleted. Useful for splitting a god-impl.
- `extract_rust_trait`: extract a trait declaration from named methods of an
  inherent `impl` block. Requires `impl_name`, `module_name` (the new trait
  name), `item_names`. Emits the trait into `target`, generates an
  `impl <Trait> for <Type>` block forwarding to the moved bodies, and reports
  object-safety hazards (`generic_methods`, `self_by_value_methods`,
  `associated_constants`, `dyn_compatible`) plus `trait_in_scope_required`
  for call sites that need to import the trait.
- `migrate_rust_type_usages`: rewrite `module_name::OldType` usages to a new
  type. Pass `module_name` (the path qualifier), `old_text` (the old simple
  name + optional position constraint such as `OldType@type_position`), and
  `new_text` (the replacement). Skipped sites surface in `migration_skipped`
  with reasons (illegal-position, ambiguous import, etc.).
- `move_rust_struct_fields`: move named fields from one struct to another
  (same file or across files). Pass `item_names` (field names),
  `impl_name`-or-`module_name` (source struct), `target` (destination file
  containing the destination struct), optional `visibility`, and
  `toml_entries={"acknowledge_repr": true}` when the source struct has a
  non-default `#[repr(...)]`. With `deep_analysis=true` reports
  `remaining_source_accessors` and `inherited_generics`.
- `add_rust_delegate_field`: add `<visibility> <delegate_field>: <delegate_type>`
  to a named struct and wire `self.<delegate_field> = <delegate_type>::new()`
  into matching constructor bodies. Constructor names default to `["new"]`;
  override via `item_names`. Custom init expression via
  `toml_entries={"init_expr": "..."}`.
- `update_rust_callers`: rewrite source-side reads/writes of moved fields or
  methods through a `delegate_field` getter/setter. Pass `delegate_field`,
  `item_names` (moved member names), optional `impl_name`/`module_name` for
  the source struct, optional `target` + `delegate_type` so the rewriter can
  consult the delegate struct for Copy-whitelist behavior. Sites it refuses
  to rewrite surface as `unrewriteable` / `overlapping` / `borrow_promotions`.
- `move_file`: rename one file to another with hash protection. No content
  rewrite, no caller updates — purely a file-system move guarded by the
  refactor envelope (sha256 check, atomic rename, rollback).
- `rust_impl_partition_analysis` (analysis-only): build the call/state graph
  of methods inside one `impl` block. Pass `source` + `impl_name` (or
  `module_name`). Returns `partition_graph`; no FileEdits. Use before a split
  to see which methods cluster together.
- `rust_public_api_guard` (analysis-only): score a proposed set of changes
  against the file's public API surface and report severity / touched-item
  delta. `toml_entries={"proposed_changes": [...]}` carries the change set.
  Returns `public_api_report`; no FileEdits.
- `rust_compile_fix_round`: classify `RustcDiagnostic` messages (captured by a
  prior compound-run command step with `capture="rustc_json"`) into
  `use-decl-add`, `visibility-rewrite`, or `replace_text` proposals. Use only
  inside `bbox_refactor_run`; the run-loop hook fetches the diagnostics from
  the capture-context ref named by `toml_entries["diagnostics_ref"]`
  (default `"last"`).
- `replace_text` (generic): exact-string replace within one file. Refuses on
  zero matches; refuses on multiple matches unless `replace_all=true`. Use
  for grounded textual residue after a semantic operation (fixture metadata,
  literal strings, dead links) — not as a substitute for `rust_lsp_rename`.
- `write_file` (generic): replace an entire file with `new_text`, or create
  it if missing. Hash-checked against the current bytes (or empty bytes for
  a new file). Used for the scaffolding-then-RA-move pattern.
- `ensure_toml_table` (generic): insert/merge a TOML table with the supplied
  `toml_table` name and `toml_entries` map. Idempotent — re-running does not
  duplicate keys. Useful for `Cargo.toml` adjustments inside a compound run.

Compound run steps:

- `{"op":"plan", ...}`: accepts the same arguments as `bbox_refactor_plan`.
  If `project_dir` is omitted in the step, the run-level `project_dir` is used.
- `{"op":"command","command":"cargo","args":["test","--bin","blackboxd"]}`:
  runs a required validation command in the run-level `project_dir` by default.
  Required command failure rolls back prior plan writes in the same run.
- `{"op":"command","command":"cargo","args":["fmt"],"touches":["src/lib.rs"]}`:
  use `touches` for mutating toolchain commands. `cargo check` and `cargo test`
  normally omit it because they validate rather than rewrite source files.

Prefer command steps inside `bbox_refactor_run` for phase gates that should
rollback together. Run additional exploratory commands outside the transaction
when they should not control rollback.

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

Delete obsolete items after a move or reparent:

```text
bbox_refactor_plan(
  kind="delete_rust_items",
  source="src/main.rs",
  item_names=["old_module"],
  item_kinds=["mod_item"],
  project_dir="/absolute/project/root"
)
```

For impl methods, pass explicit `item_names`, `item_kinds=["impl_method"]`, and
usually `impl_name`.

Add the module declaration:

```text
bbox_refactor_plan(
  kind="add_rust_mod_decl",
  source="src/main.rs",
  module_name="search_tools",
  project_dir="/absolute/project/root"
)
```

Add a re-export or import:

```text
bbox_refactor_plan(
  kind="add_rust_use_decl",
  source="src/lib.rs",
  use_path="server::BlackboxServer",
  visibility="pub",
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
uncommitted edits. For unregistered projects, pass `allow_unregistered_paths=true`
to skip project registration while keeping hash, dirty-file, parse, and
atomic-write safeguards.

DO NOT call `bbox_project_register` just to satisfy the apply path's project
check — registration triggers the project-bootstrap-arc (full indexing, chunking,
embedding) which is expensive. `allow_unregistered_paths=true` is the correct
escape hatch for ad-hoc work.

### Plan transport for large refactors

The MCP transport caps inline parameter size. For `extract_java_class`,
`extract_rust_trait`, multi-file extracts, or any plan likely to exceed a few
hundred KB of JSON, write the plan to disk and apply by path:

```text
bbox_refactor_plan(
  kind=...,
  source=...,
  output_path="my-plan.json",   # relative; resolves under $BLACKBOX_STATE_DIR/refactor/plans/
  ...
)
# Returns RefactorPlanSummary (counts only); the full plan is on disk.

bbox_refactor_apply(
  plan_path="my-plan.json",
  confirm=true,
  allow_unregistered_paths=true,  # if applicable
)
```

`plan` and `plan_path` are mutually exclusive. `plan` accepts a JSON object
returned directly from a no-`output_path` plan call; do NOT stringify it.

4. Run the Rust toolchain:

```text
cargo fmt
cargo check
cargo test --bin blackboxd
```

Use a narrower test command when the changed package has a clearer local test.

## Splitting a monster file

Don't reach for `rust_ra_move_item_to_module` first — RA's code action
declines for most realistic moves. The working sequence uses the
syntactic tools end-to-end:

1. Scaffold the destination file (`Write` or `write_file`) — empty file
   with a doc comment + `use super::*;`. For a child module of `parent.rs`,
   the file goes at `parent/<child>.rs`.
2. `add_rust_mod_decl(source="parent.rs", module_name="child")` — adds
   `mod child;` so rustc sees the new file.
3. `rewrite_rust_item_visibility(visibility="pub(super)", item_names=...)`
   — bump every item that will move so the parent module can call it
   after the move. Use `pub(crate)` instead when callers are further away.
4. `extract_rust_items(source, target, item_names, item_kinds)` — move
   the named items literally. Tree-sitter validates both files.
5. `add_rust_use_decl(source="parent.rs", use_path="child::{Name1, fn2}")`
   — bring the moved names back into scope so existing call sites compile
   without qualifier changes.
6. If the moved items are structs with private fields constructed outside
   the new module, `rewrite_rust_field_visibility(visibility="pub(super)",
   item_names=[StructName])` so the parent can still build them. Forgetting
   this surfaces as `E0451: fields ... of struct ... are private`.
7. `cargo check` + relevant test suite. Iterate if visibility errors fire.

### Inline `mod foo { ... }` → `foo.rs` submodule file

No bbox plan kind handles this transform in one shot today. `extract_rust_items`
moves the WHOLE `mod foo { ... }` block verbatim to the target, which leaves
the target with a redundant nested module. `rust_ra_move_item_to_module`
declines (see above). Until the tool catches up, do it manually:

1. Read the body content of the inline mod (lines between the opening `{`
   and closing `}`).
2. Write that body as `foo.rs` (drop one indentation level if you want,
   though rustc doesn't care).
3. Replace the inline `mod foo { ... }` block in the parent with
   `mod foo;` (preserve any `#[cfg(test)]` attribute).
4. `cargo check` + tests.

For `#[cfg(test)] mod tests { ... }` specifically this is mechanical and
safe — the tests retain access to the parent's private items via
`use super::*;` because `mod tests;` keeps the tests module nested under
the same parent.

## Safety Rules

- Treat tree-sitter success as syntax validation, not semantic proof.
- Do not use structural moves for macro-heavy code without `cargo check`.
- Moving an item with the SYNTACTIC tools (`extract_rust_items`,
  `extract_rust_impl_methods`, `lift_rust_inherent_to_free`, `move_file`,
  `move_rust_struct_fields`) does NOT rewrite cross-file callers. Pair with
  `update_rust_callers` (delegate-based), `migrate_rust_type_usages` (type
  alias migration), or follow up with `cargo check` + `rust_compile_fix_round`
  inside a compound run. The SEMANTIC tool `rust_ra_move_item_to_module`
  rewrites callers via RA's workspace edit.
- Plain `//` comments above a method are not treated as owned method trivia
  unless attached to an attribute/doc block. Convert durable method comments to
  rustdoc before moving when the comment must follow the method.
- For symbolic rename, use `rust_lsp_rename`; do not substitute
  `replace_text` unless the intended edit is genuinely a literal text rewrite
  rather than a binding-aware rename. `replace_text` is effectively
  grep/replace inside the refactor transaction envelope; it should only appear
  for grounded string literals, fixture metadata, or similar textual residue
  after the semantic operation is already done.
- For broad autonomous restructuring, use a durable bro plus reviewer loop; the
  refactor tools supply mechanical edits, not architectural judgment.
- `bbox_project_register` is NOT a workaround for the apply path's project
  check. It triggers full project indexing (chunk + embed + edge emit) which
  is expensive. Use `allow_unregistered_paths=true` instead.
- For plans whose JSON exceeds the MCP transport budget (large extracts,
  multi-file rewrites), use the `output_path` / `plan_path` round-trip.
  Stringifying a plan to pass to `bbox_refactor_apply(plan=...)` fails with
  `expected struct RefactorPlan`; pass a JSON object or use `plan_path`.

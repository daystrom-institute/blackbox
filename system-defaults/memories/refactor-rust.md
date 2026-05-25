---
tags:
  - refactor-tools
  - rust
---
+++
title = "Rust refactor mechanization — tree-sitter inventory and writable item extraction"
tags = ["refactor", "refactoring", "mechanization", "restructure", "rust", "rs", "tree-sitter", "bbox_refactor_status", "bbox_refactor_plan", "bbox_refactor_apply", "extract_rust_items", "cargo", "rust-analyzer", "symbol", "rename", "move", "extract"]
order = 8
template = false
+++
# Rust Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or splitting Rust code with
blackbox refactor tools.

## Atom signposts

For recurring Rust refactor patterns, check `atom_search(query="<intent>")`
before re-deriving the whole tool sequence. Use atoms as contextual shortcuts
for patterns such as impl partition inventory, public-API preflight, test
island extraction, state extraction, trait extraction, error migration, large
impl splitting, single-domain impl-method extraction with wiring, or bin-to-lib
module migration. The atom manifest is the source of truth for version,
inputs, cost, and prompt text; this memory keeps the primitive plan-kind map
and safety invariants.

For Rust architecture diagnosis rather than a known mechanical edit, use
`atom_search(query="rust architecture <pressure>")` or
`bbox_artifact_list(kind="workflow", name="arch-pathology-rust")` to find the
cataloged diagnostic lane. The Rust architecture workflow surveys cheap facts
first, dispatches only justified diagnostic atoms, reviews claims on a
whiteboard, and writes a correction plan for later PD dispatch. Treat those
atoms as analysis-only signposts for role/state/module/trait/error/cfg/async
runtime/test/unsafe/macro/transcript pressure; map any accepted remediation
back to the primitive plan kinds below or to an explicit manual slice.

When no atom fits the exact shape, the manual plan-kind sequence below is the
canonical path.

## Current Capability

Rust is the first writable backend. Plan dispatcher exposes the kinds below;
ask `bbox_refactor_plan` (dry-run) before `bbox_refactor_apply(confirm=true)`.

Inspect: `bbox_refactor_status`. Apply: `bbox_refactor_apply(confirm=true)`.
Compound: `bbox_refactor_run(confirm=true)` runs ordered plan + command steps
with rollback across primitive-plan file writes if a later required step fails.
When a repair command uses `on_failure="continue_for_repair"`, any later
terminal failure rolls back the whole validated transaction segment, including
plan writes that happened before the repair command.

Plan kinds, grouped by intent:

- Syntactic moves / deletes: `extract_rust_items`, `extract_rust_section`,
  `move_rust_items_with_local_deps`, `extract_rust_impl_methods`,
  `extract_rust_function_region`, `lift_rust_inherent_to_free`,
  `extract_rust_trait`, `move_rust_struct_fields`,
  `delete_rust_items`, `move_file`,
  `inline_mod_to_file_submodule` (inline `mod foo { ... }` → `foo.rs` + `mod foo;`),
  `extract_rust_items_to_submodule` (compound: scaffold + mod_decl + visibility
  bump + extract + use_decl + struct-field visibility — five primitive
  roundtrips collapsed into one plan),
  `move_rust_items_with_callers` (extract_rust_items + cross-file caller-prefix
  rewrite).
- Caller / accessor rewrites: `update_rust_callers`, `migrate_rust_type_usages`,
  `migrate_rust_string_field_to_enum`, `rewrite_rust_error_type`,
  `rust_match_arm_to_strategy`, `add_rust_delegate_field`,
  `add_rust_router_to_sum`.
- Module wiring: `add_rust_mod_decl`, `add_rust_use_decl`, `rust_module_wiring`,
  `copy_rust_mod_decls`, `rewrite_rust_bin_crate_paths`,
  `rust_minimize_imports` (conservative wildcard-import replacement).
- Visibility: `rewrite_rust_mod_visibility`, `rewrite_rust_item_visibility`,
  `rewrite_rust_field_visibility`.
- LSP-backed (rust-analyzer): `rust_lsp_rename` (semantic rename),
  `rust_organize_imports` (per-file `source.organizeImports`),
  `rust_ra_classify_callbacks` (resolve method callees via goto-definition).
  These fail closed with `error.lsp_unavailable` when rust-analyzer is missing,
  times out, or crashes; do not downgrade them to syntax-only approximations.
- LSP-backed but BROKEN: `rust_ra_move_item_to_module` — RA's `refactor.move`
  code-action kind only backs the `move_module_to_file` and `move_to_mod_rs`
  assists. Neither accepts caller-supplied destinations. The `target` param
  is title-only. Reach for `inline_mod_to_file_submodule` (inline mod case)
  or `extract_rust_items_to_submodule` / `move_rust_items_with_callers`
  (cross-file case) instead.
- Analysis only (no FileEdits): `rust_impl_partition_analysis` (impl-method
  graph for split planning), `rust_top_level_dependency_analysis` (top-level
  item graph + external reference hints + suggested clusters),
  `rust_public_api_guard` (advisory for visibility changes touching public API).
- Run-loop integration: `rust_compile_fix_round` (classify a `capture=rustc_json`
  step's diagnostics into use-decl / visibility / replace proposals),
  `split_rust_impl_methods_to_submodule` (a `bbox_refactor_run` plan-step macro
  that expands to method extraction + wiring + cargo-check repair),
  `migrate_rust_mods_to_lib` (a `bbox_refactor_run` macro for moving selected
  binary-root `mod` declarations into `src/lib.rs` and validating all bins).
- Generic primitives (language-agnostic, useful in compound runs):
  `replace_text`, `write_file`, `ensure_toml_table`.

Tree-sitter language: `rust`.

Writable plan kinds:

- `extract_rust_items`: move named top-level Rust items from one file to another.
  Optional `target_prelude` is inserted before generated target content when it
  is not already present; use it for child-module imports needed by derives and
  external crates.
- `extract_rust_section`: move every complete named top-level Rust item inside a
  marker or line-delimited source range. Bounds come from
  `toml_entries.start_marker` + `end_marker` or `start_line` + `end_line`.
  The planner refuses ranges that split a top-level item, then delegates to the
  same extraction/validation machinery as `extract_rust_items`.
- `move_rust_items_with_local_deps`: move named top-level Rust items plus private
  top-level dependencies that are exclusively referenced by the moving closure.
  Shared or externally referenced dependencies stay in the source and are
  reported in leftovers. This is syntax/index-hint closure, not full Rust name
  resolution; run `cargo check` after applying.
- `extract_rust_impl_methods`: move named methods out of one `impl` block into
  another file, preserving method attributes/modifiers such as `async` and
  optionally generating a `#[tool_router(router = name)]` wrapper around the
  moved methods. When moving from `foo.rs` to `foo/bar.rs`, the planner rebases
  `super::...` references one module deeper (`super::super::...`). If explicit
  `visibility` is supplied, it overrides every moved method. If it is not
  supplied and the parent file still references a moved method after deletion,
  only originally private moved methods are widened to `pub(super)`; existing
  `pub`, `pub(crate)`, or `pub(super)` visibility is preserved.
- `extract_rust_function_region`: conservative intra-function extraction.
  Requires exact `old_text`, helper name via `item_names[0]` or `module_name`,
  and explicit `toml_entries.parameters=["x: Type"]` /
  `toml_entries.arguments=["x"]`. Optional `toml_entries.return_type="Type"`
  controls whether the default replacement is expression-like; `new_text`
  can override the replacement call. Rejects regions containing `return`,
  `break`, `continue`, or `?` rather than trying to infer complex control
  flow or borrow behavior.
- `split_rust_impl_methods_to_submodule`: use inside `bbox_refactor_run`, not
  `bbox_refactor_plan`. Expands to `add_rust_mod_decl` (idempotent via optional
  skip), `extract_rust_impl_methods`, optional `rust_organize_imports` on the
  target, `cargo check --message-format=json` with rustc JSON capture,
  `rust_compile_fix_round`, final `cargo check`, and optional targeted tests.
  Inputs mirror `extract_rust_impl_methods`; `module_name` defaults to the
  target file stem, `target_prelude` defaults to `use super::*;`, and
  `item_kinds` defaults to `["impl_method"]`. `toml_entries` supports
  `skip_organize_imports=true` and `targeted_tests=["test_name", ...]`.
- `migrate_rust_mods_to_lib`: use inside `bbox_refactor_run`, not
  `bbox_refactor_plan`. Inputs: `source` is the binary root (for example
  `src/main.rs`), `item_names` are module declarations to move, `target`
  defaults to `src/lib.rs`, and `visibility` defaults to `pub` because binary
  crates import the package library as an external crate. The expansion copies
  selected `mod` declarations to the lib target, deletes those declarations
  from the binary root, rewrites simple bin-root `crate::<module>` references
  to `<package_crate>::<module>`, checks every Cargo `[[bin]]` target with
  rustc JSON capture, runs `rust_compile_fix_round`, then runs final
  `cargo check --bins`. `toml_entries.bin_sources=["src/other.rs"]` adds
  extra bin roots to the path-rewrite pass.
- `rewrite_rust_bin_crate_paths`: primitive used by
  `migrate_rust_mods_to_lib`; rewrites simple `crate::<module>` references and
  grouped `use crate::{module_a, module_b};` imports when every grouped entry
  is in `item_names`. Mixed grouped imports are reported in `leftovers` for
  manual cleanup after.
- `rust_module_wiring`: one conservative module-graph edit in a Rust module
  file. `toml_entries.action` supports `add_mod`, `remove_mod`, `add_use`, and
  `remove_use`; `module_name` drives mod actions and `use_path` drives use or
  reexport actions. Add actions reject duplicates, remove actions reject missing
  declarations, and every plan includes tree-sitter validation. Use it for
  repeated `mod.rs` cleanup such as adding `pub mod tools;` or removing stale
  `pub(crate) use response::*;` reexports.
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
- `rust_ra_move_item_to_module`: **BROKEN; prefer the alternatives below.**
  Requests rust-analyzer's `refactor.move` code action. That kind backs
  only two RA assists: `move_module_to_file` (inline `mod foo { ... }` →
  `foo.rs`) and `move_to_mod_rs` (file → `mod.rs` migration). NEITHER
  accepts a caller-supplied destination — RA picks the target itself. The
  tool's `target` parameter is title-only; it is NOT sent to RA. In
  observed practice (rust-analyzer 1.95) the action does not fire for
  top-level items moved between sibling modules, nor for inline `mod foo
  { ... }` blocks. Expect `error.lsp_unavailable: no move-to-module code
  action found for <name>` for most realistic uses.
  Replace with: `inline_mod_to_file_submodule` (inline-mod-to-file) or
  `extract_rust_items_to_submodule` / `move_rust_items_with_callers`
  (cross-file moves). Accepts `function_item`, `struct_item`,
  `enum_item`, `trait_item`, `type_item`, `const_item`, `static_item`,
  `mod_item`. Refuses `impl_method`.
- `inline_mod_to_file_submodule`: extract the body of an inline
  `mod foo { ... }` block into a sibling submodule file, replacing the
  block with `mod foo;`. Outer attributes (`#[cfg(test)]`, doc comments)
  stay attached to the retained declaration. Target path auto-derives:
  `parent.rs` → `parent/<name>.rs`; `lib.rs` / `main.rs` / `mod.rs` →
  `<name>.rs` (flat sibling). Explicit `target` overrides. Refuses
  non-empty existing targets and refuses already-file submodule
  declarations (`mod foo;` with no body). Body de-indentation strips the
  longest run of leading spaces common to every non-blank line; tabs
  pass through verbatim (`cargo fmt` after).
- `extract_rust_items_to_submodule`: compound plan that collapses the
  five-step ceremony for splitting a Rust module into one plan. Does
  scaffolded target + `mod <module_name>;` insertion + visibility bump
  on every moved item AND its struct fields + extract + `use
  <module_name>::{...};` re-import in the parent. `visibility` defaults
  to `pub(super)`. `module_name` defaults to the target file stem.
  `target_prelude` defaults to `use super::*;`. Visibility transforms
  are baked into the target FileEdit's replacement text, not emitted as
  separate ordering-dependent edits. Refuses `impl_method` (use
  `extract_rust_impl_methods`). Idempotent on the source's `mod`/`use`
  decls — re-running over an already-wired parent doesn't duplicate
  them.

  Extra knobs via `toml_entries`:
  - `use_decl_visibility` (default `private`): visibility of the
    re-export. Set to `pub(crate)` when the parent uses `use foo::*;`
    glob to bring moved entry-points into a dispatcher's scope — the
    private default keeps the import scoped to the parent file only.
  - `use_decl_items` (default = auto-detect): explicit subset of
    `item_names` to re-export. The default auto-prunes the use_decl to
    only those moved names whose simple identifier still appears
    somewhere in the source after the deletions land. Names whose only
    references were inside the moved items themselves are dropped from
    the use_decl entirely (no spurious unused-import warning).
    When `use_decl_visibility="pub(super)"` and `use_decl_items` is omitted,
    the planner emits `pub(super) use <module_name>::*;` as an explicit broad
    compatibility mode for sibling modules that rely on `use super::*`.
  - `merge_into_existing_target` (default `false`): append moved item
    blocks to an existing non-empty target instead of refusing. Useful
    for incremental batching multiple plan calls into the same
    submodule file. When true, the target's prelude is preserved as-is
    and new blocks are concatenated at the end with blank-line
    separators.
- `move_rust_items_with_callers`: extracts items AND walks the project
  rewriting cross-file callers. For each moved item, every
  `<source_simple>::<item>` occurrence in any other `.rs` file gets the
  prefix segment rewritten to `<target_simple>`, including occurrences
  inside `use` declarations. Word-boundary checked (`mod_ax::moved` is
  left alone). `module_name` overrides the source simple-name default
  (file stem); `target_prelude` overrides the target simple-name
  default. Current limits: simple-name segment match only (FQN that skip the
  source module aren't matched), multi-import use trees not split,
  no alias awareness. Pair with `extract_rust_items_to_submodule` when
  you also need visibility bumps + a `use` decl in the source's parent.
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
- `rewrite_rust_error_type`: rewrite selected function signatures from one
  error type to another and rewrite mapped construction sites. Pass `old_text`
  (from type), `new_text` (to type), `item_names` (functions whose signatures
  may change), and optional `toml_entries.error_mapping`. Public error types
  require `toml_entries={"acknowledge_public_api_change": true}` as explicit
  operator authority; the plan reports `question_mark_sites` so `?`
  conversions can be repaired through cargo-check plus `rust_compile_fix_round`.
- `rust_match_arm_to_strategy`: generate per-variant strategy modules and a
  router function for a match-on-enum shape. Pass `module_name` as the enum
  name, `item_names` as behavior-family method names, and optional
  `toml_entries.data_field_names`, `driver_share_groups`, or `driver_name`.
  Variants carrying associated data are refused in `refused_variants` instead
  of guessed.
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
- `migrate_rust_string_field_to_enum`: conservative helper for one struct
  field at a time. `toml_entries.field_name` names the `String` field,
  `toml_entries.enum_name` or `module_name` names the generated enum, and
  `toml_entries.variants=[{"name":"Variant","rename":"wire", "aliases":[...]}]`
  defines serde-compatible variants. The generated enum derives
  `Serialize`, `Deserialize`, and `schemars::JsonSchema`, includes serde
  `rename`/`alias` attributes, and provides `as_str()` so existing
  `field.as_str()` matches keep compiling while follow-up match-arm rewrites
  are staged.
- `move_file`: rename one file to another with hash protection. No content
  rewrite, no caller updates — purely a file-system move guarded by the
  refactor envelope (sha256 check, atomic rename, rollback).
- `rust_impl_partition_analysis` (analysis-only): build the call/state graph
  of methods inside one `impl` block. Pass `source` + `impl_name` (or
  `module_name`). `impl_name` accepts either the simple type name
  (`"BlackboxServer"`) or the status-style impl header emitted by
  `bbox_refactor_status` (`"impl BlackboxServer"`). Returns
  `partition_graph`; no FileEdits. Use before a split to see which methods
  cluster together.
- `rust_top_level_dependency_analysis` (analysis-only): build a dependency
  graph for named or all top-level Rust items in one file. Returns
  `top_level_dependency_graph` with item->call/type/module/global edges,
  crate-wide textual external reference hints, suggested connected clusters,
  and macro-heavy warnings. Use before choosing `item_names` for
  `extract_rust_items_to_submodule`.
- `rust_minimize_imports`: replace resolvable wildcard imports such as
  `use super::*;` or `use super::helpers::*;` with explicit local names that
  are directly referenced by the file. The planner resolves local `self`,
  `super`, and `crate` module paths, intersects exported top-level names with
  identifier references, and leaves ambiguous cases in `leftovers` instead of
  guessing. `toml_entries.allow_wildcards=["crate::prelude::*"]` preserves
  intentional preludes; `remove_unused_wildcards=true` permits deleting a
  wildcard where no direct name references were found. Chain
  `rust_organize_imports` after apply when rust-analyzer is available.
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

### Case 1: extract a cluster of top-level items into a new submodule file

One plan: `extract_rust_items_to_submodule`. Inputs:

```text
bbox_refactor_plan(
  kind="extract_rust_items_to_submodule",
  source="src/refactor/java.rs",
  target="src/refactor/java/cross_file.rs",
  item_names=["MovedStaticItem", "compute_cross_file_static_caller_edits",
              "compute_static_qualifier_rewrite_edits"],
  item_kinds=["struct_item", "function_item"],
  project_dir="/abs/project/root",
  # defaults: visibility="pub(super)", module_name=target_stem,
  # target_prelude="use super::*;"
)
```

The plan covers: scaffold target + `mod cross_file;` in parent + visibility
bumps (item + struct fields) + extract + `use cross_file::{...};` in parent.
Apply, `cargo check`, done.

If callers in OTHER project files reference the moved items via
`<old_module_simple>::<Item>`, run `move_rust_items_with_callers`
separately to rewrite them — `extract_rust_items_to_submodule` only
handles the parent's own call sites.

### Case 2: inline `mod foo { ... }` → `foo.rs` submodule file

One plan: `inline_mod_to_file_submodule`. Inputs:

```text
bbox_refactor_plan(
  kind="inline_mod_to_file_submodule",
  source="src/refactor/java.rs",
  item_names=["tests"],
  # target defaults to src/refactor/java/tests.rs
  project_dir="/abs/project/root",
)
```

Pulls the body of the inline mod into a sibling submodule file, replaces
the block with `mod tests;`, preserves outer attributes (`#[cfg(test)]`,
doc comments). Operator gets a 6k-line file split in one apply.

### Case 3: move items between modules with workspace-wide caller rewrite

One plan: `move_rust_items_with_callers`. Same shape as Case 1 but walks
the whole project and rewrites every `<source_simple>::<item>` occurrence
in any `.rs` file. Current support covers the simple-name path segment match; complex
use-tree splitting and FQN paths that skip the source segment require
manual cleanup after.

### When to fall back to the syntactic primitives

The original 5-step ceremony (`add_rust_mod_decl` →
`rewrite_rust_item_visibility` → `extract_rust_items` → `add_rust_use_decl`
→ `rewrite_rust_field_visibility`) is still the right escape hatch when:

- You want different visibility per moved item (the compound primitive
  applies one visibility to everything).
- The target file already has handwritten content that should be
  preserved (the compound primitive refuses non-empty targets).
- You're moving impl methods — use `extract_rust_impl_methods` for that
  case; the compound primitive refuses `impl_method` kind.

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

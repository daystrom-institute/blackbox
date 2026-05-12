# Refactor Mechanization Catalog

Use this memory first when you know the task is restructuring but have not yet
picked a language-specific runbook.

## Support Matrix

Project-scoped symbol lookup uses `bbox_code_symbols`. Single-file syntax
exploration uses `bbox_code_node_describe` and `bbox_code_query`. Refactor
inventory uses `bbox_refactor_status` and is available for any source file
whose extension maps to a supported tree-sitter parser in `CodeChunker`.
The operational runbooks below cover the common application languages;
additional mapped languages are inspect-only unless a newer language memory
says otherwise.

`bbox_code_symbols`, `bbox_code_node_describe`, and `bbox_code_query` are syntax
locators, not refactor planners. Use `bbox_code_symbols` instead of shell
`rg -n` when you need method/function/type line numbers, candidate files for a
symbol, or exact refactorable item names. Use `bbox_code_node_describe` when you
have a line/column and need local grammar shape. Use `bbox_code_query` when the
file is known and you need a custom tree-sitter pattern. Their responses include
a `handoff` block with suggested `bbox_refactor_status` and
`bbox_refactor_project_refs` calls. Treat those suggestions as the bridge into
the guarded refactor surfaces; do not turn raw query captures directly into
edits unless the edit is a generic literal plan such as `replace_text`.

`bbox_refactor_project_refs` is a grounding-only companion for metadata and
provenance repairs. It returns current
`project_file:<project>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>` refs for
one file using the same chunking/hash rules as the agentic corpus. Use it before
literal edits to eval fixtures, citations, or expected refs; whole-file
`sha256sum` is not a valid substitute for a chunk hash.

- Rust: `rust`
- TypeScript / TSX: `typescript`
- JavaScript / JSX / MJS / CJS: `javascript`
- Python: `python`
- C#: `csharp`
- Java: `java`
- Go: `go`
- C: `c`
- C++: `cpp`
- Additional inspect-only parser mappings: Erlang, Elixir, Ruby, OCaml,
  Haskell, Swift, Kotlin, Scala, Lua, Bash, JSON, YAML, TOML, HTML, CSS, SQL

Writable structural plans are narrower:

- Generic:
  - `bbox_refactor_plan(kind="move_file")` moves a file from `source` to a
    missing `target`. Supported source files are syntax-validated at the
    destination path; unsupported file types still get hash, dirty-file,
    path-scope, target-exists, and rollback checks.
  - `bbox_refactor_plan(kind="replace_text")` replaces exact `old_text` with
    `new_text` in `source`. By default the old text must match exactly once;
    pass `replace_all=true` for every occurrence. This is a literal edit
    primitive with transaction/rollback guardrails, not a semantic refactor
    primitive. Treat it as grep-with-safety for hard-coded metadata, generated
    fixtures, or other already-grounded literals. Do not present it as symbolic
    rename, import repair, extraction, or move support.
  - `bbox_refactor_plan(kind="write_file")` replaces or creates `source` with
    complete `new_text`. Supported source files are parse-validated.
  - `bbox_refactor_plan(kind="ensure_toml_table")` ensures a top-level TOML
    table named by `toml_table` contains `toml_entries` values. This is for
    simple structured config edits such as adding `[lib]` to a manifest.
  Use `bbox_refactor_apply(confirm=true)` for one plan or
  `bbox_refactor_run(confirm=true)` when several primitive plans and command
  validations must succeed or rollback together.
  In `bbox_refactor_run` command steps, `command` is the executable only and
  arguments go in `args`: use `{"command":"cargo","args":["fmt"]}`, not
  `{"command":"cargo fmt"}`.
  Command steps have three failure modes via `on_failure` (RX-F2b):
  - `"required"` (default): exit-code != 0 rolls back and terminates.
  - `"optional"`: exit-code != 0 is logged and the run continues.
  - `"continue_for_repair"`: exit-code != 0 opens a repair obligation and
    continues. A later step (e.g. `rust_compile_fix_round`) must mark the
    obligation `Consumed` or `LeftOver`. Any obligation still `Open` at run
    end triggers rollback from the first soft-fail cursor. Consumed and
    LeftOver obligations remain live rollback anchors until terminal success
    — only reaching the end of the run without any failure releases them.
  The `on_failure` field supersedes the legacy `required: bool` when set.
  Canonical repair sequence: `[extract_plan, add_mod_decl, cargo_check
  (continue_for_repair, capture=rustc_json), rust_compile_fix_round, cargo_check
  (required=true), cargo_test (required=true)]`.
- Rust: `bbox_refactor_plan(kind="extract_rust_items")` or
  `bbox_refactor_plan(kind="extract_rust_impl_methods")` or
  `bbox_refactor_plan(kind="delete_rust_items")` or
  `bbox_refactor_plan(kind="add_rust_router_to_sum")` or
  `bbox_refactor_plan(kind="add_rust_mod_decl")` or
  `bbox_refactor_plan(kind="add_rust_use_decl")` or
  `bbox_refactor_plan(kind="copy_rust_mod_decls")` or
  `bbox_refactor_plan(kind="rewrite_rust_mod_visibility")` or
  `bbox_refactor_plan(kind="rewrite_rust_item_visibility")` or
  `bbox_refactor_plan(kind="rewrite_rust_field_visibility")` or
  `bbox_refactor_plan(kind="rust_lsp_rename")` or
  `bbox_refactor_plan(kind="rust_organize_imports")`, then
  `bbox_refactor_apply(confirm=true)`. Use `bbox_refactor_run(confirm=true)`
  when several primitive plans must succeed or rollback together. The
  rust-analyzer-backed plan kinds (`rust_lsp_rename`, `rust_organize_imports`)
  go through the warm `LspSessionManager`: first call per project pays the
  cold-start cost, subsequent calls reuse the same `(project_root, Rust)`
  child until idle eviction (`BLACKBOX_LSP_IDLE_SECS`). Tunables:
  `RUST_ANALYZER_BIN` (binary override) and
  `BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS` (default 60).
- TypeScript / JavaScript: inspect-only today. Use `sm-refactor-typescript`.
- C#: inspect-only today. Use `sm-refactor-csharp`.
- Python: inspect-only today. Use `sm-refactor-python`.
- Java: supports extract methods/classes, composite extract-class handoffs, field/constructor/delegate wiring, caller delegation, extract interface, add implements, visibility rewriting, type-use migration, and JDTLS/fallback import organize. Use `sm-refactor-java`.
- Go: inspect-only today. Use `sm-refactor-go`.
- C / C++: inspect-only today. Use `sm-refactor-c-cpp`.
- Other supported tree-sitter languages: inspect-only today unless a newer
  language memory says otherwise.

## Routing

- Rust files (`.rs`) -> pull `sm-refactor-rust`.
- TypeScript or JavaScript files (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`)
  -> pull `sm-refactor-typescript`.
- C# files (`.cs`) -> pull `sm-refactor-csharp`.
- Python files (`.py`) -> pull `sm-refactor-python`.
- Java files (`.java`) -> pull `sm-refactor-java`.
- Go files (`.go`) -> pull `sm-refactor-go`.
- C/C++ files (`.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx`) ->
  pull `sm-refactor-c-cpp`.
- Mixed-language projects -> pull every relevant language memory, then plan one
  language backend at a time.

## Common Protocol

1. Find symbols and line ranges structurally before reaching for `rg`:

```text
bbox_code_symbols(
  project_dir="/absolute/project/root",
  query="methodOrTypeName",
  languages=["java"],
  item_kinds=["method_declaration"],
  limit=20
)
```

Use this for the common "where is this method?" and "what line range is this
symbol on?" cases. The response returns `file`, `kind`, `name`, `byte_range`,
`line_range`, truncation metadata, and handoff calls. The default scan budget is
large enough for normal monorepos; if `truncated=true`, narrow with
`path_contains`, `languages`, `item_kinds`, or pass a more deliberate
`file_limit`. `rg` is still fine for unsupported file types, literal
prose/config search, or broad text audits, but it should not be the first tool
for supported source-code symbol line numbers.

2. Explore local syntax when the target is unclear:

```text
bbox_code_node_describe(
  file="path/to/file",
  project_dir="/absolute/project/root",
  line=42,
  column=12,
  include_text=true,
  include_siblings=true
)
```

Use `bbox_code_node_describe` to learn the local grammar: node kind, named
fields, parent chain, siblings, parse health, and the nearest refactor-like
ancestor. Then use `bbox_code_query` for broader single-file pattern searches:

```text
bbox_code_query(
  file="path/to/file",
  project_dir="/absolute/project/root",
  query="(function_item name: (identifier) @name)",
  limit=50,
  include_text=true
)
```

Code-nav output is `semantic_status="syntax_only"`. It may locate syntax that
looks relevant, but it does not prove binding, import paths, macro expansion, or
type correctness. Follow the response's `handoff.refactor_status` suggestion
when you need a refactorable item name/kind; follow
`handoff.project_refs` when you need current `project_file` entity refs.

3. Inspect refactorable items before planning:

```text
bbox_refactor_status(
  file="path/to/file",
  project_dir="/absolute/project/root",
  item_kinds=["impl_method"],
  limit=50,
  include_attributes=false
)
```

For Rust, status includes top-level items plus `impl_method` entries so agents
can copy exact method names before planning an impl extraction. Omit filters for
small files only; status defaults to at most 200 returned items and reports
`total_items`, `matching_items`, `returned_items`, and `truncated`.

4. Only call `bbox_refactor_plan` for a generic plan kind listed here or for a
   language-scoped plan kind that the language memory says is writable.

5. Only call `bbox_refactor_apply` after reviewing the JSON plan. Apply requires
   `confirm=true`, registered-project path scope, clean git files by default
   unless `allow_dirty_worktree=true`, hash checks, non-overlapping edits, parse
   validation, and atomic writes. For disposable practice worktrees or isolated
   smoke tests, `allow_unregistered_paths=true` bypasses the registered-project
   requirement without disabling hash, syntax, or dirty-file checks.
   Missing target files are not inherently dirty: supported extraction plans
   that create a target should model that as an empty-original `FileEdit`.
   Do not pre-create placeholder files or pass `allow_dirty_worktree=true` just
   to let an extraction produce a new type.

   **Plan-file slot policy (RX-F1b).** `output_path` (planner write) and
   `plan_path` (applier read) both resolve under
   `$BLACKBOX_STATE_DIR/refactor/plans/`. Pass a plain relative filename such as
   `"my-plan.json"` — not an absolute path. Absolute paths and filenames that
   escape the slot via `../` traversal are rejected with
   `error.bad_input(code=plan_path_outside_slot)`. The `plan_path` value in the
   returned `RefactorPlanSummary` is the absolute on-disk path; pass only the
   filename portion (e.g. `Path::file_name`) when calling apply.

6. Run the language toolchain after apply. Tree-sitter proves syntax shape, not
   semantic binding. For compound phases, add command validation steps directly
   to `bbox_refactor_run`:

```text
{"op":"command","command":"make","args":["test"],"required":true}
{"op":"command","command":"make","args":["format"],"touches":["src/example.ext"],"required":true}
```

   The runner executes commands in `project_dir` by default. Command steps are
   validation-only unless `touches` declares paths they may mutate. Declared
   touches are snapshotted before the command and are rolled back with prior
   plan writes on required command failure.

7. Dispatched agents normally cannot see `bbox_refactor_*` because those tools
   are in the default recursion guard. The orchestrator must deliberately use
   `allow_recursion=true` when delegating a refactor task that needs these tools.

## Refactor-atom personas (RA-B1 / RA-B2)

The atomic refactor agent layer (`design/refactor-agents.md`) ships JSON
manifests installed via `bbox_artifact_install(kind="agent", …)`. Every
refactor atom binds to a **narrow persona brofile** whose allow list is
restricted to the refactor + grounding tool surface. The narrow-allow
constraint is load-bearing: `MergedFilters::merge` is additive with
deny-wins, so an atom's `filter_overlay` can only ADD denies on top of
the brofile — it cannot narrow a permissive allow list. A refactor
atom layered on top of a general persona has no mechanical surface
restriction.

Two personas ship as reference artifacts:

- **`rust-refactor-persona`** at `examples/brofiles/rust-refactor-persona.json`.
  Allows only `bbox_code_*`, `bbox_refactor_*` (status / project_refs /
  plan / apply / run), `bbox_note`, `bbox_thread`, `bbox_pin`,
  `bbox_inspect_entity`, `bbox_hybrid_search`, `Read`, `Grep`, `Glob`.
  Disallows `Bash`, `Write`, `Edit`, `bbox_learn` / `bbox_remember` /
  `bbox_decide` / `bbox_forget` / `bbox_render`, and `bro_*`. Cargo
  validation runs through `bbox_refactor_run` command steps, never via
  `Bash`. The brofile spec is verified by
  `rust_refactor_persona_matches_design_spec` in
  `src/orchestration/brofile.rs`; allow/disallow drift fails the test.

- **`java-refactor-persona`** — same shape, Java-flavored lens. Ships
  only when Java atoms (RA-X1) are introduced.

Refactor-atom manifests under `examples/agents/refactor/*.json` MUST
bind to one of these personas via `brofile_ref`. Authoring an atom that
binds to a different brofile (e.g., `code-reviewer-persona`) means the
atom's tool surface is not actually narrowed; the manifest lint pass
(RA-S1) rejects such manifests on install.

## Refactor atom catalog

Shipped refactor atoms (RA-A* / RA-X*) — discover via
`bro_agent_search(<intent phrase>)`, install via
`bbox_artifact_install(kind="agent", source="examples/agents/refactor/<name>.json")`,
dispatch via `bro_agent_dispatch(agent="<name>", args={...})`.

| Atom | Status | Cost | Plan kind(s) | Purpose |
|---|---|---|---|---|
| `rust-impl-partition-graph` | shipped (v1, RA-A1) | cheap | `rust_impl_partition_analysis` (RX-G1) | Produce method/field/call graph for a Rust impl block; analysis-only |
| `rust-public-api-guard` | shipped (v1, RA-A2) | normal | `rust_public_api_guard` (RX-G2) | Report public-API delta of a proposed refactor as advisory severity; preflight for mutating atoms |
| `rust-test-island-extract` | shipped (v1, RA-A3) | normal | `extract_rust_items`, `add_rust_mod_decl`, `rust_compile_fix_round` | Peel inline #[cfg(test)] mod tests blocks into sibling src/tests/*.rs files |
| `rust-state-extract` | shipped (v1, RA-A4) | normal | `extract_rust_items`, `move_rust_struct_fields`, `add_rust_delegate_field`, `update_rust_callers`, `rust_compile_fix_round` | Pull a self.<field> cluster into a separate struct + delegate; operator-authority `acknowledge_repr` for #[repr(C)] / #[repr(packed)] |
| `rust-trait-from-impl` | shipped (v1, RA-A5) | normal | `extract_rust_trait`, `migrate_rust_type_usages`, `rust_compile_fix_round` | Lift method subset into trait + impl Trait for Struct; HARD REFUSES migrate_call_sites=true when dyn_compatible=false |

Per-atom manifests live at `examples/agents/refactor/<atom>.json`. The
shared prompt template and base outputs schema (RA-T1) under the same
directory are reference files — manifest installers inline the
filled-in form. The CI catalog-completeness check (RA-D1, follow-up)
will assert every `examples/agents/refactor/<name>.json` has a row
here.

## Refactor-atom install lint (RA-S1)

A manifest is treated as a refactor atom — and subject to the refactor-atom
lint — when one of:

- Top-level `"_contract": "refactor-atom/v1"` field is present (authoring
  convention; refactor atom templates include this).
- Artifact source path contains `examples/agents/refactor/`.

There is no opt-out flag in v1: an operator who wants different semantics
either drops the contract marker or hosts the manifest outside the refactor
path. `schema/agent.schema.json` has `additionalProperties: false` at the
manifest level, so adding a top-level escape-hatch field would itself be
rejected by the generic schema.

**Hard rejects** (install fails with
`error.bad_input(code=refactor_atom_lint_failed)`):

- `brofile_ref` is not one of `rust-refactor-persona` /
  `java-refactor-persona`. Refactor atoms layered on a permissive persona
  have no mechanical tool-surface restriction — `filter_overlay` can only
  ADD denies.
- `inputs.schema` declares an `acknowledge_*` field with a `default` value
  (any default). Operator-authority opt-outs must be operator-explicit per
  RX-V1.

**Warnings** (install succeeds, surfaced via `install_warnings`):

- `filter_overlay.allow` non-empty — refactor atoms narrow via additional
  denies only.
- `outputs.schema` drops one or more RA-T1 base fields (status, plan_path,
  files_touched, fixme_count, deep_analysis_summary, cargo_result,
  block_reason, done_note_id).
- `inputs.prompt_template` is missing one or more protocol markers
  (`bbox_refactor_plan`, `bbox_refactor_run`, `bbox_note(kind=`).
  Analysis-only atoms legitimately skip `bbox_refactor_run`; the warning
  is informational in that case.

## Shared atom contract (RA-T1)

Every refactor atom embeds the same five-step protocol
(ground → plan with `deep_analysis=true` → decide → apply-or-block →
done-note) and the same base outputs.schema (status / plan_path /
files_touched / fixme_count / deep_analysis_summary / cargo_result /
block_reason / done_note_id). Reference files:

- `examples/agents/refactor/_template.prompt.md` — the prompt template
  with `{{...}}` placeholders. Atom manifests inline the filled-in form
  under `inputs.prompt_template`. The artifact installer does not yet
  support shared-template includes; the `tools/refactor-atom-fill`
  helper (follow-up) keeps manifests in sync mechanically.
- `examples/agents/refactor/_base.outputs.schema.json` — the base
  outputs.schema every atom unions with atom-specific fields. Advisory
  in v1: dispatch does not validate the agent's emission against the
  schema; the RA-S1 lint warns when a manifest's `outputs.schema`
  drops one or more of the base fields.

`fixme_count` is split into `plan_only` (FIXMEs the plan emitted with
no associated edit) and `warning` (FIXMEs emitted alongside an applied
edit, flagging operator follow-up) per the two-prefix FIXME grammar in
`sm-refactor-rust`.

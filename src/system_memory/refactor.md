# Refactor Mechanization Catalog

Use this memory first when you know the task is restructuring but have not yet
picked a language-specific runbook.

## Support Matrix

Inspection uses `bbox_refactor_status` and is available for any source file
whose extension maps to a supported tree-sitter parser in `CodeChunker`. The
operational runbooks below cover the common application languages; additional
mapped languages are inspect-only unless a newer language memory says otherwise.

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
- Rust: `bbox_refactor_plan(kind="extract_rust_items")` or
  `bbox_refactor_plan(kind="extract_rust_impl_methods")` or
  `bbox_refactor_plan(kind="delete_rust_items")` or
  `bbox_refactor_plan(kind="add_rust_router_to_sum")` or
  `bbox_refactor_plan(kind="add_rust_mod_decl")` or
  `bbox_refactor_plan(kind="add_rust_use_decl")` or
  `bbox_refactor_plan(kind="copy_rust_mod_decls")` or
  `bbox_refactor_plan(kind="rewrite_rust_mod_visibility")` or
  `bbox_refactor_plan(kind="rewrite_rust_item_visibility")`, then
  `bbox_refactor_apply(confirm=true)`. Use `bbox_refactor_run(confirm=true)`
  when several primitive plans must succeed or rollback together.
- TypeScript / JavaScript: inspect-only today. Use `sm-refactor-typescript`.
- C#: inspect-only today. Use `sm-refactor-csharp`.
- Python: inspect-only today. Use `sm-refactor-python`.
- Java: inspect-only today. Use `sm-refactor-java`.
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

1. Inspect first:

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

2. Only call `bbox_refactor_plan` for a generic plan kind listed here or for a
   language-scoped plan kind that the language memory says is writable.

3. Only call `bbox_refactor_apply` after reviewing the JSON plan. Apply requires
   `confirm=true`, registered-project path scope, clean git files by default
   unless `allow_dirty_worktree=true`, hash checks, non-overlapping edits, parse
   validation, and atomic writes. For disposable practice worktrees or isolated
   smoke tests, `allow_unregistered_paths=true` bypasses the registered-project
   requirement without disabling hash, syntax, or dirty-file checks.

4. Run the language toolchain after apply. Tree-sitter proves syntax shape, not
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

5. Dispatched agents normally cannot see `bbox_refactor_*` because those tools
   are in the default recursion guard. The orchestrator must deliberately use
   `allow_recursion=true` when delegating a refactor task that needs these tools.

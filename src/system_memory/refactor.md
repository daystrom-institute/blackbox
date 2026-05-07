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

- Rust: `bbox_refactor_plan(kind="extract_rust_items")` or
  `bbox_refactor_plan(kind="extract_rust_impl_methods")` or
  `bbox_refactor_plan(kind="add_rust_router_to_sum")`, then
  `bbox_refactor_apply(confirm=true)`.
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
bbox_refactor_status(file="path/to/file", project_dir="/absolute/project/root")
```

For Rust, status includes top-level items plus `impl_method` entries so agents
can copy exact method names before planning an impl extraction.

2. Only call `bbox_refactor_plan` for a plan kind that the language memory says
   is writable.

3. Only call `bbox_refactor_apply` after reviewing the JSON plan. Apply requires
   `confirm=true`, registered-project path scope, clean git files by default
   unless `allow_dirty_worktree=true`, hash checks, non-overlapping edits, parse
   validation, and atomic writes.

4. Run the language toolchain after apply. Tree-sitter proves syntax shape, not
   semantic binding.

5. Dispatched agents normally cannot see `bbox_refactor_*` because those tools
   are in the default recursion guard. The orchestrator must deliberately use
   `allow_recursion=true` when delegating a refactor task that needs these tools.

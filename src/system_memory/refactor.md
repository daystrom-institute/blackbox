# Refactor Mechanization Catalog

Use this memory first when you know the task is restructuring but have not yet
picked a language-specific runbook.

## Support Matrix

Inspection uses `bbox_refactor_status` and is available for any source file
whose extension maps to a supported tree-sitter parser in `CodeChunker`:

- Rust: `rust`
- TypeScript / TSX: `typescript`
- JavaScript / JSX / MJS / CJS: `javascript`
- Python: `python`
- C#: `csharp`
- Java: `java`
- Go: `go`
- C: `c`
- C++: `cpp`

Writable structural plans are narrower:

- Rust: `bbox_refactor_plan(kind="extract_rust_items")`, then
  `bbox_refactor_apply(confirm=true)`.
- TypeScript / JavaScript: inspect-only today. Use `sm-refactor-typescript`.
- C#: inspect-only today. Use `sm-refactor-csharp`.
- Other supported tree-sitter languages: inspect-only today unless a newer
  language memory says otherwise.

## Routing

- Rust files (`.rs`) -> pull `sm-refactor-rust`.
- TypeScript or JavaScript files (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`)
  -> pull `sm-refactor-typescript`.
- C# files (`.cs`) -> pull `sm-refactor-csharp`.
- Mixed-language projects -> pull every relevant language memory, then plan one
  language backend at a time.

## Common Protocol

1. Inspect first:

```text
bbox_refactor_status(file="path/to/file", project_dir="/absolute/project/root")
```

2. Only call `bbox_refactor_plan` for a plan kind that the language memory says
   is writable.

3. Only call `bbox_refactor_apply` after reviewing the JSON plan. Apply requires
   `confirm=true`, registered-project path scope, clean git files by default
   unless `allow_dirty_worktree=true`, hash checks, non-overlapping edits, parse
   validation, and atomic writes.

4. Run the language toolchain after apply. Tree-sitter proves syntax shape, not
   semantic binding.

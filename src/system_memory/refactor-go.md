# Go Refactor Mechanization Runbook

Use this memory before operating on Go files with blackbox refactor tools.

## Current Capability

Go is an inspect-first backend today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no Go-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use gopls or another
  Go-aware refactoring workflow.
- Import repair: not automatic; use `gofmt` / `goimports` and the Go toolchain.

Tree-sitter language: `go`.

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="pkg/name/file.go",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this to
map packages, imports, const/var/type/function declarations, methods, and
candidate extraction ranges.

2. Search and inspect neighbors:

```text
bbox_hybrid_search(
  query="type or function name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

3. Make edits with the normal code editing path, then validate with project
commands:

```text
gofmt -w <files>
go test ./...
go vet ./...
```

Use `goimports` when imports may have changed.

## Safety Rules

- Do not apply Rust plan kinds to Go files.
- Tree-sitter does not resolve package imports, build tags, generated files,
  interfaces, method sets, or module replacement semantics.
- For rename/move operations, prefer gopls-backed edits and then run `go test`.


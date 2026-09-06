+++
title = "C and C++ refactor mechanization — tree-sitter inventory and clang validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "c", "cpp", "c++", "clangd", "clang-rename", "clang-tidy", "clang-format", "cmake", "ninja", "tree-sitter", "bbox_refactor_status", "symbol", "rename", "move", "extract"]
order = 16
template = false
+++
# C and C++ Refactor Mechanization Runbook

The daemon refactor MCP surface is retired. `bbox_refactor_*` and `bbox_code_*`
spellings below identify historical engine operations, not callable MCP tools.
Use the current harness catalog (`isolate --list`, then `isolate --describe <tool>`)
for exact native names and schemas. Compose operations in the caller; atom and
workflow wrappers are retired. Plan kinds and safety invariants below remain
reference material where the native binding uses that engine.

Use this memory before operating on C or C++ files with blackbox refactor tools.

## Current Capability

C and C++ are inspect-first backends today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no C/C++-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use clangd, clang-rename,
  clang-tidy, or IDE tooling.
- Include repair: not automatic; use the project build and formatter.

Tree-sitter languages:

- `.c`, `.h` -> `c`
- `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx` -> `cpp`

Many C++ projects use `.h` for C++ headers. Blackbox currently maps `.h` to the
C parser, so C++ syntax such as classes, templates, and namespaces in `.h` files
may report `parse.has_error=true`. Treat that as an extension/parser mismatch
and validate with clang tooling rather than trusting the C parse.

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="src/file.cpp",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this to
map declarations, definitions, namespaces, classes/structs, functions, macros,
and candidate extraction ranges.

2. Search and inspect neighbors:

```text
bbox_hybrid_search(
  query="symbol or header name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

3. Make edits with the normal code editing path, then validate with project
commands. Common commands:

```text
clang-format -i <files>
cmake --build <build-dir>
ctest --test-dir <build-dir>
ninja -C <build-dir> test
make test
```

Use the build system actually configured in the repository.

## Safety Rules

- Do not apply Rust plan kinds to C or C++ files.
- Tree-sitter does not resolve preprocessor conditionals, include paths,
  templates, overload sets, macros, ABI boundaries, or generated headers.
- For rename/move/extract, prefer clangd/clang tooling and compile after every
  structural edit.

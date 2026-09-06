+++
title = "Python refactor mechanization — tree-sitter inventory and Pyright/Rope validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "python", "py", "pyright", "jedi", "rope", "ruff", "pytest", "tree-sitter", "bbox_refactor_status", "symbol", "rename", "move", "extract"]
order = 11
template = false
+++
# Python Refactor Mechanization Runbook

The daemon refactor MCP surface is retired. `bbox_refactor_*` and `bbox_code_*`
spellings below identify historical engine operations, not callable MCP tools.
Use the current harness catalog (`isolate --list`, then `isolate --describe <tool>`)
for exact native names and schemas. Compose operations in the caller; atom and
workflow wrappers are retired. Plan kinds and safety invariants below remain
reference material where the native binding uses that engine.

Use this memory before operating on Python files with blackbox refactor tools.

## Current Capability

Python is an inspect-first backend today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no Python-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use Pyright, Jedi, Rope, or
  another language-server/refactoring workflow.
- Import repair: not automatic; use the project formatter, linter, typechecker,
  and tests.

Tree-sitter language: `python`.

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="src/package/module.py",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this to
map modules, imports, classes, functions, and candidate extraction ranges.

2. Search and inspect neighbors:

```text
bbox_hybrid_search(
  query="class or function name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

3. Make edits with the normal code editing path, then validate with project
commands. Common commands:

```text
ruff format
ruff check
pyright
pytest
```

Use the tooling actually configured in the repository.

## Safety Rules

- Do not apply Rust plan kinds to Python files.
- Treat tree-sitter inventory as syntax context, not binding semantics. It does
  not resolve dynamic imports, monkeypatching, descriptors, metaclasses, or
  runtime module loading.
- For rename/move/extract, prefer language-server or Rope-backed workflows and
  then run tests.


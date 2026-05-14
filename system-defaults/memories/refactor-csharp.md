+++
title = "C# refactor mechanization — tree-sitter inventory and Roslyn validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "csharp", "c#", "cs", "roslyn", "omnisharp", "tree-sitter", "bbox_refactor_status", "symbol", "rename", "move", "extract", "dotnet"]
order = 10
template = false
+++
# C# Refactor Mechanization Runbook

Use this memory before operating on C# files with blackbox refactor tools.

## Current Capability

C# is an inspect-first backend today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no C#-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use Roslyn, OmniSharp, Rider,
  Visual Studio, or another C# language-server workflow.
- Import repair: not automatic; use the project formatter, compiler, and test
  suite.

Tree-sitter language: `csharp`.

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="src/Path/File.cs",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this to
map classes, interfaces, records, namespaces, and candidate extraction ranges.

2. Search and inspect neighbors:

```text
bbox_hybrid_search(
  query="class or method name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

Use `bbox_inspect_entity` and `bbox_find_paths` when answering questions about
where a symbol is defined or referenced in the indexed graph. Bundle evidence
before making provenance-sensitive claims.

3. Make edits with the normal code editing path, then validate with project
commands. Common commands:

```text
dotnet format
dotnet build
dotnet test
```

Use solution- or project-specific arguments when the repository defines them.

## Safety Rules

- Do not apply Rust plan kinds to C# files.
- Treat tree-sitter inventory as syntactic context, not Roslyn semantics. It
  does not resolve partial classes, generated code, extension methods, using
  aliases, nullable flow analysis, or project references.
- For symbolic rename, extract interface, move type, or namespace changes, use
  Roslyn-backed tooling or compiler-verified manual edits.
- For generated files, prefer editing the source generator input rather than
  moving generated output.


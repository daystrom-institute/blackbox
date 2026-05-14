+++
title = "TypeScript and JavaScript refactor mechanization — tree-sitter inventory and validation workflow"
tags = ["refactor", "refactoring", "mechanization", "restructure", "typescript", "javascript", "tsx", "jsx", "tsserver", "tree-sitter", "bbox_refactor_status", "symbol", "rename", "move", "extract", "typecheck"]
order = 9
template = false
+++
# TypeScript and JavaScript Refactor Mechanization Runbook

Use this memory before operating on TypeScript, TSX, JavaScript, JSX, MJS, or CJS
files with blackbox refactor tools.

## Current Capability

TypeScript and JavaScript are inspect-first backends today.

- Inspect: supported with `bbox_refactor_status`.
- Plan/apply: no TypeScript-specific mutation plan is currently supported.
- Semantic rename: not supported by blackbox yet; use the TypeScript language
  server, project IDE tooling, or `tsserver`-backed editor commands.
- Import repair: not automatic; use the project formatter/linter/typechecker.

Tree-sitter languages:

- `.ts`, `.tsx` -> `typescript`
- `.js`, `.jsx`, `.mjs`, `.cjs` -> `javascript`

## Tool Sequence

1. Inventory a file:

```text
bbox_refactor_status(
  file="src/path/file.ts",
  project_dir="/absolute/project/root"
)
```

The response includes parse health, language, file hash, top-level node kinds,
names where tree-sitter exposes them, byte ranges, and line ranges. Use this for
symbol extraction, move planning, review packets, and blast-radius notes.

2. Search and inspect neighbors before editing:

```text
bbox_hybrid_search(
  query="symbol or module name",
  project="/absolute/project/root",
  doc_type="project_file",
  vector_weight=0.0
)
```

Use `bbox_inspect_entity` and `bbox_find_paths` when a claim depends on indexed
graph relationships. Bundle evidence before answering provenance-sensitive
questions.

3. Make edits with the normal code editing path, then validate with project
commands. Common commands:

```text
npm test
npm run typecheck
npm run lint
npm run format
pnpm test
pnpm typecheck
pnpm lint
pnpm format
```

Use the package manager and scripts actually present in the repo.

## Safety Rules

- Do not apply Rust plan kinds to TypeScript or JavaScript files.
- Treat `bbox_refactor_status` output as structural context, not a binding
  graph. It does not know module resolution, path aliases, JSX transforms,
  ambient declarations, or type-only imports.
- For rename or move, prefer language-server support so references, imports,
  barrel exports, and path aliases are updated coherently.
- For framework code, validate with the framework’s test/build command after
  the edit, not just the TypeScript compiler.


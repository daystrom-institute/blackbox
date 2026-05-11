# Code Navigation and Symbolic Exploration

Date: 2026-05-09
Status: proposal, revised after code-grounding review

## Problem

Agents exploring a codebase still spend too much context on `rg`, broad
full-file reads, and hand-written guesses about AST shape. That is wasteful and
fragile: comments, strings, similarly named symbols, nested declarations, and
language-specific syntax all pollute text search.

Blackbox is not starting from zero. The daemon already has:

- tree-sitter parsers for the core code languages plus language-pack mappings
- source chunking that extracts symbol-like AST nodes
- indexed `project_file` chunks with `symbol` and `symbol_exact` fields
- typed `symbol` entities
- AST graph edges such as `DEFINED_IN`, `CONTAINS_SYMBOL`, `CALLS`,
  `USES_TYPE`, `HAS_FIELD`, and `IMPLEMENTS_TRAIT`
- `bbox_refactor_*` tools for parse health, source inventory, guarded edits,
  and Rust / Java language-scoped refactor plans

The actual gap is narrower and more useful: there is no ergonomic read-side
surface for live tree-sitter queries, AST node inspection at a position, or
kind-filtered symbol navigation. The indexed graph can already answer some
symbol questions, but agents need simpler tools and better metadata to use it
without hand-assembling entity refs and graph traversals.

## Design Boundary

Keep three surfaces separate.

| Surface | Authority | Good for | Not good for |
|---|---|---|---|
| Live tree-sitter syntax | current file bytes | AST queries, node ranges, syntactic captures, declaration inventory | binding resolution, imports, macro expansion, type inference |
| Indexed graph | last indexed project corpus | definitions, symbol chunks, derived call/type edges, evidence bundles | stale-until-reindex facts, grammar-level custom queries |
| LSP / compiler | language server and build toolchain | workspace rename, find references, import organization, type-aware checks | cheap ad hoc syntax scans |

Tree-sitter is the locator and syntax validator. The graph is the indexed
navigation layer. LSP/compiler feedback is the semantic authority.

Any tool that returns syntactic references must label them as syntax-derived or
heuristic. Any tool that promises binding-aware answers must either route
through LSP/compiler-backed behavior or use graph edges while exposing their
stored confidence.

## Current State

### Exposed Tools

| Tool | Current role |
|---|---|
| `bbox_refactor_status` | Parse health and syntax item inventory for one source file. Rust has deeper method inventory; other languages currently use direct top-level named children. |
| `bbox_refactor_project_refs` | Recomputes current `project_file:<project>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>` refs for one file using the same chunking rules as indexing. |
| `bbox_refactor_plan` | Produces guarded structural plans. Includes generic text/file plans, Rust-specific plans, Java-specific plans, `rust_lsp_rename`, and import organization surfaces. |
| `bbox_refactor_apply` | Applies reviewed plans with hash checks, dirty-file guardrails, registered-project scoping, atomic writes, and parse validation. |
| `bbox_refactor_run` | Runs ordered primitive plans plus validation commands with rollback across touched files. |
| `bbox_hybrid_search` | Searches indexed `project_file` chunks, including symbol/path fields. Useful for "where is X?" but not a dedicated symbol API. |
| `bbox_inspect_entity` / `bbox_find_paths` | Navigates typed graph entities and AST edges once an entity ref is known. |

### Code-Grounded Substrate

- `src/chunker/code.rs` maps extensions to language names and obtains
  tree-sitter parsers through `tree_sitter_language_pack::get_parser` with
  direct grammar fallbacks for Rust, Python, C#, Java, Go, TypeScript,
  JavaScript, C, and C++.
- `CodeChunker::chunk` parses source, extracts `SymbolSpec` records with
  qualified and bare names, and emits code chunks with `symbol` and
  `symbol_exact`.
- `src/index/project_files.rs` builds a project-wide symbol table from those
  chunks and emits `DEFINED_IN`, `CONTAINS_SYMBOL`, `HAS_FIELD`,
  `IMPLEMENTS_TRAIT`, `CALLS`, and `USES_TYPE` edges.
- Current call/type/field/trait edges are derived from chunk text and headers,
  not from a full language semantic model. They are useful graph hints, not
  binding authority.
- `Chunk` does not currently store the tree-sitter node kind for a symbol.
  Kind-filtered indexed search therefore requires a schema/data-model change,
  not only a new tool wrapper.

## Real Gaps

1. **No arbitrary tree-sitter query tool.** Agents cannot run grammar-native
   S-expression queries such as "all `unsafe` blocks", "all `#[test]`
   functions", or "all calls whose function node has this shape".

2. **No node-at-position introspection.** Agents cannot ask "what AST node am I
   on, what are its named fields, and what is its parent chain?" This makes
   query authoring dependent on prior grammar knowledge.

3. **No ergonomic indexed symbol search.** The index can surface symbol-bearing
   chunks through hybrid search, but there is no dedicated tool that filters by
   language, symbol name, file, or node kind and returns stable navigation
   fields.

4. **No persisted `symbol_kind`.** `bbox_code_symbols(kind="struct_item")`
   cannot be index-first until `SymbolSpec`, `Chunk`, Tantivy fields, embedding
   source docs, and project refs include the tree-sitter node kind.

5. **No live syntax reference extractor.** Intra-file calls, imports, field
   accesses, and identifier occurrences can be extracted syntactically, but they
   must be presented as syntax facts, not "find references".

6. **Inventory depth is uneven.** `bbox_refactor_status` is deep for Rust and
   shallower for other languages. The chunker already does recursive symbol
   walking; the read-side inventory tool should reuse that rather than expose
   only top-level nodes.

## Proposed Tools

### Priority 1: `bbox_code_query`

Run a tree-sitter query against one source file.

```text
bbox_code_query(
  file: String,
  query: String,
  project_dir: Option<String>,
  language: Option<String>,
  limit: Option<usize>,
  include_text: Option<bool>
)
```

Return capture records:

- capture name
- node kind
- byte range and line/column range
- matched text excerpt when requested
- immediate parent kind
- parse report for the file
- `semantic_status: "syntax_only"`

V1 should be single-file only. Multi-file query execution belongs behind a
registered-project and file-size gate after the single-file API is proven.

### Priority 2: `bbox_code_node_describe`

Describe the smallest named node at a source position.

```text
bbox_code_node_describe(
  file: String,
  line: usize,
  column: usize,
  project_dir: Option<String>,
  include_siblings: Option<bool>,
  include_text: Option<bool>
)
```

Return:

- selected node kind, byte range, line/column range, and named field-in-parent
- node text excerpt when requested
- named children with field names where available
- parent chain up to the root
- optional previous/next named sibling summaries
- parse report

This is the tool agents use to discover grammar shape before writing
`bbox_code_query` patterns.

### Priority 3: `bbox_code_symbols`

Provide a dedicated symbol search surface over project code.

```text
bbox_code_symbols(
  project_dir: String,
  query: Option<String>,
  language: Option<String>,
  kind: Option<String>,
  file: Option<String>,
  mode: Option<"indexed" | "live">,
  limit: Option<usize>
)
```

V1 options:

- `mode="indexed"` searches existing project-file docs and supports name,
  language, file, and path filtering. It must reject `kind` until `symbol_kind`
  is indexed.
- `mode="live"` reparses matching project files and can support `kind`, but
  must be capped by registered project, supported extension, file size, and
  result limit.

V2 should add `symbol_kind` to indexed chunks and make indexed `kind` filtering
the default.

### Priority 4: `bbox_code_refs`

Extract syntactic references from one file.

```text
bbox_code_refs(
  file: String,
  project_dir: Option<String>,
  kind: "calls" | "imports" | "fields" | "identifiers" | "all",
  query: Option<String>,
  limit: Option<usize>
)
```

Return syntax-derived facts only:

- reference kind
- displayed name/text
- node kind
- byte and line/column range
- containing symbol when cheaply available from parent chain
- `semantic_status: "syntax_only"` or `edge_confidence: "heuristic"`

This is not a replacement for LSP find-references. It is a cheap local scanner
for "what syntax appears in this file?"

### Explicit Non-Tool: Generic `rename_symbol`

Do not add `bbox_refactor_plan(kind="rename_symbol")` as a generic surface.
That name implies binding-aware behavior.

Use existing language-backed rename where available, currently
`bbox_refactor_plan(kind="rust_lsp_rename")`. If a declaration-only rewrite is
later useful, name it plainly, for example
`rewrite_declaration_name_in_file`, and return
`semantic_status: "structural_only"`.

## Data Model Changes

To make indexed symbol search honest and useful, add:

- `SymbolSpec.kind: String`
- `Chunk.symbol_kind: Option<String>`
- Tantivy stored/indexed field `symbol_kind`
- project refs output field `symbol_kind`
- embedding source docs that include `symbol_kind`
- schema version bump and reindex requirement

The existing `symbol` field should remain the qualified display name. The
existing `symbol_exact` field should remain the bare lookup token. `symbol_kind`
must be the tree-sitter node kind, such as `struct_item`,
`function_definition`, or `class_declaration`.

## Implementation Plan

### Phase 1: Shared Code Navigation Module

Create `src/code_nav/` or equivalent and reuse existing parsing helpers rather
than duplicating them.

Responsibilities:

- resolve project-relative file paths
- enforce file-size and registered-project gates for multi-file work
- parse source with the same language mapping as `CodeChunker`
- produce parse reports
- convert byte ranges to line/column ranges
- expose node text excerpts safely with limits

Add `bbox_code_query` and `bbox_code_node_describe`, plus tool docs and tests.

### Phase 2: Symbol Kind Persistence

Extend the chunk/index data path:

1. Add `kind` to `SymbolSpec`.
2. Carry it into `Chunk.symbol_kind`.
3. Store it in Tantivy and source-doc views.
4. Return it from `bbox_refactor_project_refs`.
5. Bump the project-file parser/schema version.
6. Add tests showing indexed symbols can be filtered by kind after reindex.

### Phase 3: Symbol Search Tool

Add `bbox_code_symbols`.

Start with `mode="indexed"` using existing fields plus `symbol_kind` once
available. Add `mode="live"` only if there is a concrete use case that justifies
reparsing many files on demand.

### Phase 4: Syntax Reference Extraction

Add `bbox_code_refs` as a per-file syntax scanner. Prefer grammar queries for
well-known languages where practical, but do not collapse syntax matches into
semantic claims. If a reference can be matched to an indexed symbol, return that
as an optional graph hint with confidence, not as binding truth.

## Relationship to Existing Navigation

| Question | Use |
|---|---|
| "Where is symbol X defined?" | `bbox_code_symbols` or `bbox_hybrid_search`, then `bbox_inspect_entity` |
| "What calls this indexed symbol?" | `bbox_inspect_entity(symbol:...)` or `bbox_find_paths(edge_types="CALLS")`, noting graph confidence |
| "What syntax is under my cursor?" | `bbox_code_node_describe` |
| "Find this AST pattern in one file" | `bbox_code_query` |
| "What imports/calls appear in this file?" | `bbox_code_refs` |
| "Rename this Rust symbol across the workspace" | `bbox_refactor_plan(kind="rust_lsp_rename")` |
| "Rewrite only this declaration's spelling" | Future structural-only declaration rewrite, not generic `rename_symbol` |

## Acceptance Criteria

- Tool docs explain syntax-only vs graph vs LSP semantics.
- Query and node-describe tools return bounded responses with parse diagnostics.
- Kind-filtered indexed symbol search is not exposed until `symbol_kind` exists.
- Existing graph navigation remains the preferred route for indexed callers,
  callees, and evidence bundles.
- Generic structural declaration rewrites are not named like semantic rename.
- Tests cover at least Rust, JavaScript/TypeScript, Python, and Java query or
  node-describe behavior, plus one unsupported-language error path.

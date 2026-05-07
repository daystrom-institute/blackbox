# AST-Assisted Refactor Mechanization

Date: 2026-05-07
Status: proposal, revised after Claude review
Review baseline: current working tree on 2026-05-07; `src/main.rs` 18,054
lines, `src/packets.rs` 6,365 lines, tool routers at `src/main.rs:3140` and
`src/main.rs:5261`.

## Problem

`design/restructure.md` proposes a large but mostly mechanical crate split:
moving binary-owned modules into a new library crate, splitting `main.rs` into
`server/` and `tools/`, and converting `packets.rs` into a module directory.
Doing that by hand is possible, but the review burden is high: large byte
ranges move, `impl` blocks get split, imports drift, macro call sites move, and
syntax mistakes are easy to miss until `cargo test`.

The crate already depends on tree-sitter:

- `tree-sitter = "0.26"`
- `tree-sitter-language-pack = "1.8.0-rc.26"`
- `tree-sitter-rust = "0.24.2"`
- direct grammar crates for Python, C#, Java, Go, TypeScript, JavaScript, C,
  and C++

The current code chunker parses source with `tree_sitter_language_pack::process`
and obtains a parser via `get_parser()` or direct grammar fallback. It walks the
tree to identify symbol-like nodes and emits code chunks with symbol metadata.
The project index then builds symbol edges from those chunks, but several edges
are still regex or string-header heuristics rather than AST-derived facts.

This gives us enough syntax infrastructure to mechanize structural parts of the
restructure, but not enough to promise semantic refactoring by itself.

## Boundary

Tree-sitter is a concrete syntax engine. It can identify syntactic units, byte
ranges, node kinds, named fields, parent/child nesting, imports, symbols, parse
diagnostics, and query captures. It cannot answer whether an identifier
reference is semantically bound to a specific Rust item across modules, imports,
trait method dispatch, macro expansion, type inference, or re-exports.

Use tree-sitter for:

- locating declarations and whole syntactic units
- extracting exact byte ranges for move/copy/delete edits
- grouping items by attributes, names, containing `impl`, or module location
- producing dry-run edit plans
- validating that edited files still parse without `ERROR` or `MISSING` nodes
- indexing structural facts for agents and review tools

Use rust-analyzer or another language server for:

- workspace-safe rename
- find references / go to definition
- import insertion or cleanup with semantic path resolution
- macro-aware Rust behavior
- type-directed moves and visibility repair

The practical model is: tree-sitter is the cutter, splicer, and syntax
validator; LSP/compiler feedback is the semantic authority.

## Non-Goals

V1 mutation is Rust-only and structural; V1 inspection is multi-language across
the tree-sitter grammars already exposed by the code chunker. The distinction is
intentional: inventories, parse diagnostics, node ranges, and symbol-like
top-level items are generic; safe moves, import repair, visibility changes, and
reference rewrites are language-specific.

V1 is not a replacement for the AST graph in `src/index/project_files.rs`, and
it does not attempt to improve `CALLS`, `USES_TYPE`, `HAS_FIELD`, or
`IMPLEMENTS_TRAIT` edge derivation. Those are indexing concerns, not
refactor-plan concerns.

V1 does not perform semantic rename. If rust-analyzer is unavailable, the
planner may produce a rename manifest, but it must label the plan
`semantic_status = "unverified"` and must not apply reference edits.

V1 does not perform automatic import repair. The planner may emit a manifest of
probable missing or unused imports, but cargo/rust-analyzer or a human owns the
repair. This keeps the first implementation honest: tree-sitter can move whole
syntax units, but it cannot determine canonical import paths.

V1 is not an MCP operator surface. It should start as an internal library module
plus test harness or local CLI. Add `bbox_refactor_*` only after at least one
real restructure extraction succeeds end to end.

## Current Surface

Current implementation strengths:

- `src/chunker/code.rs` maps file extensions to language names.
- It can parse supported files with tree-sitter and falls back to direct grammar
  crates for the core static language set.
- It extracts `SymbolSpec { qualified_name, bare_name, byte_start, byte_end }`
  from AST nodes, with a `StructureItem` fallback from the language pack.
- Chunks carry `symbol` and `symbol_exact`.
- `src/index/project_files.rs` builds a project-wide symbol table and emits
  `DEFINED_IN`, `CONTAINS_SYMBOL`, `HAS_FIELD`, `IMPLEMENTS_TRAIT`, `CALLS`,
  and `USES_TYPE` edges.
- `tree_sitter_language_pack::ProcessResult` already exposes `imports`,
  `exports`, `symbols`, `diagnostics`, `structure`, and `chunks`. V1 can use
  those for manifests and validation hints, but not as semantic truth.

Current limitations:

- `CodeChunker::chunk` returns no AST edges directly; edges are derived later
  from chunks.
- `CALLS` and `USES_TYPE` are regex-derived from chunk content.
- `IMPLEMENTS_TRAIT` and `HAS_FIELD` use string heuristics.
- Symbol identity is chunk-hash based, so renames produce new symbol IDs. That
  is correct for indexing history, but refactor plans must not persist only
  indexed entity refs because those refs become stale after the move.
- Tree-sitter node kinds and field names are grammar-version sensitive. V1
  selectors are pinned to the current Rust grammar crate
  `tree-sitter-rust = "0.24.2"` and must be fixture-tested before any grammar
  upgrade.

## Proposed Refactor Layer

Add an internal module for syntax-guided refactor planning:

```
src/refactor/
  mod.rs
  syntax.rs          # parse file, language selection, query helpers
  plan.rs            # RefactorPlan, FileEdit, TextEdit, conflict checks
  languages/         # language-specific selectors and edit recipes
    rust.rs
  validate.rs        # parse validation + cargo/rust-analyzer validation hooks
  apply.rs           # dry-run rendering, backups, temp writes, rollback
```

Initial public API, internal to the crate:

```rust
pub struct SyntaxFile {
    pub path: PathBuf,
    pub language: String,
    pub source: String,
    pub tree: tree_sitter::Tree,
}

pub struct SyntaxItem {
    pub plan_local_id: String,    // path + original byte range + kind + name
    pub kind: String,
    pub name: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub parent_path: Vec<String>,
    pub leading_trivia_start: usize,
    pub trailing_trivia_end: usize,
    pub attributes: Vec<String>,
}

pub struct RefactorPlan {
    pub title: String,
    pub edits: Vec<FileEdit>,
    pub validations: Vec<ValidationStep>,
    pub semantic_status: SemanticStatus,
}

pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

pub struct TextEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
}

pub enum ValidationStep {
    TreeSitterNoErrors { path: PathBuf, byte_range: Option<(usize, usize)> },
    CargoCheck { command: Vec<String> },
    CargoTest { command: Vec<String> },
    RustAnalyzerRename { symbol: String },
}

pub enum SemanticStatus {
    StructuralOnly,
    LspVerified,
    Unverified,
}
```

Rules:

- Plans are dry-run first. Applying a plan requires explicit caller intent.
- `bbox_refactor_status` is language-generic for supported tree-sitter
  grammars and returns parse health plus syntactic inventory. Rust includes
  top-level items and direct `impl_method` entries.
- `bbox_refactor_plan` is a dispatcher: each plan kind declares the language
  backend it uses. Unsupported language/kind pairs fail closed.
- Edits in one file must be non-overlapping and applied from high byte offset to
  low byte offset.
- Every plan parses the current working tree from disk. It must not trust the
  project-file index because the index can lag the worktree.
- Plan records use path + original byte range + item kind + item name, not only
  `project_file:` or `symbol:` entity refs. `plan_local_id` is valid only
  against the exact source snapshot used to build the plan.
- Every changed supported source file is reparsed after edits. Validation fails
  if the tree contains `ERROR` nodes or `MISSING` nodes in the modified range.
- Macro-bearing extractions require `cargo check` or `cargo test`; syntax-only
  success is not enough.
- A semantic rename plan must require rust-analyzer/LSP confirmation before
  editing references.

## Language System Memories

Language-specific detail belongs in system memories, not in a custom
refactoring DSL. Agents should pull the catalog first, then the relevant
runbook before operating:

- `sm-refactor`
- `sm-refactor-rust`
- `sm-refactor-typescript`
- `sm-refactor-csharp`

`sm-refactor` is a routing catalog and support matrix. Each language memory
records the current support matrix for that language: supported MCP tools,
required arguments, valid plan kinds, tree-sitter language/model name,
validation commands, and semantic caveats. This lets a Rust restructuring agent
load Rust-specific item kinds and `cargo` expectations, while a C# + TypeScript
agent loads Roslyn/tsserver expectations and sees that those languages are
currently inspect-only for blackbox mutation.

The refactor API should remain language-neutral at the entry point:

- `bbox_refactor_status` answers "what syntax inventory can tree-sitter expose
  for this file?"
- `bbox_refactor_plan` dispatches to explicit language/kind backends.
- `bbox_refactor_apply` applies hash-checked plans with parse validation.

Adding a new language backend means adding or updating that language's system
memory in the same patch as the backend, so agents do not have to infer support
from source code.

Current Rust writable plan kinds:

- `extract_rust_items`: move named top-level Rust items between files.
- `extract_rust_impl_methods`: move named methods from a single Rust `impl`
  block into a generated target `impl` wrapper, preserving method attributes
  and optionally adding `#[tool_router(router = name)]`.
  If a matching target impl already exists, the planner appends into it instead
  of creating another sibling impl. The `#[tool_router(router = name)]` shape is
  intentionally coupled to the current `rmcp` macro syntax used by
  `BlackboxServer`; future macro changes require updating this generator.
- `add_rust_router_to_sum`: append `+ Self::<router_name>()` to a Rust
  `tool_router:` field initializer so generated tool routers become reachable.

## Tree-Sitter Recipes

### Locate Rust Items

Use Rust grammar node kinds and fields to locate:

- `mod_item`
- `use_declaration`
- `struct_item`
- `enum_item`
- `trait_item`
- `function_item`
- `impl_item`
- `macro_definition`
- `attribute_item`

Selectors must be fixture-tested against `tree-sitter-rust = "0.24.2"`.
Upgrading the Rust grammar requires rerunning selector fixtures and updating
node-kind assumptions before refactor plans are trusted again.

### Leading Trivia Attachment

Rust outer attributes and doc comments are usually siblings before the item, not
children of the item. Each item selector returns two starts:

- `byte_start`: the syntactic item start
- `leading_trivia_start`: the start including attached outer attributes and doc
  comments
- `trailing_trivia_end`: the deletion boundary after the moved item, including
  enough whitespace to leave the source with one coherent blank-line gap

Attachment policy:

- Include immediately preceding `#[...]` outer attributes if no non-comment,
  non-whitespace token separates them from the item.
- Include immediately preceding `///` or `//!` doc comments if zero blank lines
  separate the comment block from the item or attached attributes.
- Include contiguous ordinary `//` comments only when they are directly adjacent
  to an attached attribute/doc block; otherwise leave them behind.
- Preserve exactly one blank line between moved items in the destination.
- When deleting the source item, consume trailing whitespace through the next
  blank line if one exists; otherwise consume only the item's syntactic range.
  This prevents doubled blank lines without eating the next item.

This policy is load-bearing for `#[tool_router]`, `#[tool]`, `#[derive(...)]`,
`#[cfg(test)]`, serde attributes, tracing attributes, and rustdoc.

### Split `main.rs`

The lib reparent from `design/restructure.md` is the real proof-of-concept, not
a later `server/progress.rs` extraction. It has two distinct edit categories:

1. Module ownership migration:
   - enumerate every top-level `mod foo;` declaration in `main.rs`
   - generate matching `pub mod foo;` declarations in `lib.rs`
   - delete those `mod` declarations from `main.rs`
2. Inline content move:
   - locate `SharedState`, `BlackboxServer`, all `impl BlackboxServer` blocks,
     `impl ServerHandler`, free helper functions, route handlers, Tail SSE, and
     daemon bootstrap helpers
   - move all non-`main` top-level items into `server/mod.rs`
   - leave binary `main.rs` with only logging/bootstrap glue and
     `blackbox::server::run().await`

The planner should build one dry-run plan containing both categories, then apply
them as one atomic filesystem operation. Intermediate states are not expected to
compile or even resolve module ownership correctly. The plan is accepted only
after parse validation and cargo validation on the final state.

### Extract Tool Domains

Tool extraction can be made mostly mechanical:

- locate `impl BlackboxServer` blocks with `#[tool_router(router = ...)]`
- inside them, locate methods with `#[tool(...)]`
- classify methods by handler name prefix (`bbox_search`, `bbox_context`,
  `bro_exec`, `badgey_exec`, etc.)
- move handler methods and DTO structs selected by an explicit manifest into
  `tools/<domain>.rs`
- wrap each target set in a new `impl BlackboxServer` block with its own
  `#[tool_router(router = <domain>_tools)]`
- update the router sum in `BlackboxServer::new`

The implemented `extract_rust_impl_methods` primitive covers the method move and
target wrapper generation, including appending into an existing matching target
impl for repeated extraction waves. `add_rust_router_to_sum` covers the router
constructor wiring. Module declarations, imports, and DTO/helper moves remain
separate mechanical steps that must be planned explicitly so the agent can
review the blast radius.

The DTO move is manifest-driven, not inferred by loose token search. The
planner may list probable DTO/helper dependencies, but the accepted move set is
explicit. V1 should expect import cleanup to be manual or LSP-assisted after the
move.

### Split `packets.rs`

For `packets.rs`, tree-sitter can classify and move whole Rust items:

- AST predicate types and parser helpers -> `packets/ast.rs`
- compilation and normalization -> `packets/compile.rs`
- evaluator functions -> `packets/apply.rs`
- fidelity audit types/functions -> `packets/audit.rs`
- scanner/repair candidates -> `packets/scanner.rs`
- event logging -> `packets/events.rs`
- stringified JSON coercion -> `packets/coerce.rs`

The first pass should use explicit selector allowlists and a dry-run manifest,
not inferred semantic dependency closure. After each move, `cargo test` remains
the gate.

### Move Tests With Code

Test relocation is heuristic unless rust-analyzer confirms bindings:

- locate the bottom `#[cfg(test)] mod tests`
- locate individual `#[test]` functions
- group tests by explicit manifest first, then by references to moved item names
- move assigned tests into the target module's local `#[cfg(test)] mod tests`
- create `packets/test_support.rs` for fixtures referenced by multiple groups
- write unassigned tests into a plan `leftovers` report

Ambiguous tests must not be silently moved. A test whose identifier tokens match
multiple target modules stays in the parent or is assigned by manifest.

The `packets.rs` split has a smaller target set and can use layer-oriented test
passes. The `main.rs` test block spans many more domains, so test relocation
should be per-domain and partial: move tests only for the extraction currently
being applied, write unassigned tests to `leftovers`, and avoid one giant
test-move plan for all `tools/*` domains.

## Macro Safety

Tree-sitter validates syntax, not macro expansion. The planner must flag any
move containing these attributes as macro-bearing:

- `#[tool_router(...)]`
- `#[tool(...)]`
- `#[derive(...)]`
- `#[serde(...)]`
- `#[tracing::instrument]`
- `#[cfg(...)]` / `#[cfg_attr(...)]`
- `#[tokio::main]`

Macro-bearing plans require `cargo check` or `cargo test` before they are
marked successful. If `cargo expand` is available, the planner may include it as
an optional diagnostic step, but cargo remains the required gate.

## Rename and Reference Plans

Tree-sitter can safely rename only declaration-local syntax targets, such as:

- renaming a module file path and its adjacent `mod foo;` declaration
- renaming a generated router function name in one syntactic block
- renaming private helper functions when all references are in the same parsed
  syntactic scope and verified by cargo

Workspace symbolic rename must be delegated:

1. Ask rust-analyzer for definition and references.
2. Convert returned ranges into a `RefactorPlan`.
3. Apply non-overlapping text edits.
4. Reparse changed files and reject `ERROR` / `MISSING` nodes.
5. Run `cargo check` or tests.

If rust-analyzer is unavailable, the planner can propose a rename manifest but
must not claim it is semantic-safe.

## Tooling Surface

Eventually expose a small MCP or CLI surface, but keep it internal until the
planner proves useful:

```
bbox_refactor_plan(kind="split-main", dry_run=true)
bbox_refactor_plan(kind="extract-tool-domain", domain="bbox_search")
bbox_refactor_plan(kind="split-packets-layer", layer="ast")
bbox_refactor_apply(plan_id="...")
bbox_refactor_status(plan_id="...")
```

The first implementation can be a local CLI or test-only harness instead of a
daemon tool. The important artifact is a reviewable plan:

```json
{
  "title": "extract bbox_search tools",
  "semantic_status": "structural_only",
  "edits": [
    {
      "path": "src/server/mod.rs",
      "edits": [
        { "byte_start": 12345, "byte_end": 15678, "replacement": "" }
      ]
    },
    {
      "path": "src/tools/bbox_search.rs",
      "edits": [
        { "byte_start": 0, "byte_end": 0, "replacement": "..." }
      ]
    }
  ],
  "validations": [
    { "tree_sitter_no_errors": { "path": "src/server/mod.rs" } },
    { "tree_sitter_no_errors": { "path": "src/tools/bbox_search.rs" } },
    { "cargo_check": { "command": ["cargo", "check", "--bin", "blackboxd"] } }
  ],
  "leftovers": []
}
```

Apply is transactional at the file-set level: before writing, the planner stores
original bytes for every touched file, writes replacements to sibling temp
files, fsyncs files and parent directories where supported, then renames temp
files over targets. If any write, parse validation, or cargo validation fails,
the apply path restores the original bytes for every touched file and reports
the failed validation step.

## Migration Plan

1. Extract a reusable syntax wrapper from `src/chunker/code.rs` into
   `src/refactor/syntax.rs` or a shared `src/syntax.rs`.
2. Add Rust selector fixtures for top-level items, attributes, doc comments,
   names, and containing blocks.
3. Implement `RefactorPlan`, `FileEdit`, `TextEdit`, dry-run rendering, and
   conflict checks.
4. Implement parse validation that rejects `ERROR` and `MISSING` nodes in
   modified ranges.
5. Prove the planner on a checked-in fixture mini-crate before touching the real
   crate.
6. Build the lib-reparent dry-run plan from `design/restructure.md` step 1.
7. Apply the lib-reparent plan only after dry-run review; gate it with
   tree-sitter validation and `cargo test`.
8. Add extraction recipes for `server/state.rs`, `server/progress.rs`, and one
   self-contained tool domain.
9. Add rust-analyzer-backed rename/reference edits only after structural moves
   are reliable.

Before the crate has a `[lib]` target, planner tests run through the binary
crate with `cargo test --bin blackboxd`. After the reparent lands, planner tests
can move to normal library and integration tests.

## Planner Tests

- Unit fixtures: small Rust files with known item ranges for top-level items,
  nested `impl`s, outer attributes, doc comments, `#[cfg(test)]` tests, and
  macro-bearing methods.
- Edit fixtures: apply planned non-overlapping edits and assert final source
  equals an expected file.
- Parse fixtures: deliberately malformed edits must produce an `ERROR` or
  `MISSING` validation failure.
- Mini-crate integration: apply a small extraction to a checked-in fixture crate
  and assert `cargo check` passes.
- Regression fixtures: pin `tree-sitter-rust = "0.24.2"` node-kind behavior so
  grammar upgrades fail loudly.

## Acceptance Gates

- A dry-run plan identifies the exact top-level items needed for the `main.rs`
  reparent step and separates module-declaration migration from inline-content
  movement.
- Applying a small structural extraction produces Rust parse trees with no
  `ERROR` or `MISSING` nodes in changed ranges before cargo runs.
- The planner refuses overlapping byte edits.
- Macro-bearing plans require cargo validation before success.
- The planner refuses semantic rename without LSP/compiler confirmation.
- At least one extraction from `restructure.md` is performed through the planner
  and passes `cargo test`.

## Open Questions

- Should applied plans be recorded into bbox notes/provenance after v1, so
  future agents can trace which refactor operation moved a range? V1 should not
  persist plan IDs beyond the local apply report.
- How much of the import manifest should be generated from
  `ProcessResult.imports` / `exports` versus cargo diagnostics?

# Code Navigation and Symbolic Exploration — implementation skeleton

Date: 2026-05-11
Status: proposal

Companion to `design/proposed/code-nav-symbolic-exploration.md`. Phases are
prefixed `CN-` to disambiguate from `RA-` (refactor-agents), `RX-` (Rust
expansion), and other plan prefixes.

Each phase names a discrete implementation chunk: scope, realizes,
components, gates, follow-ups. Phases are dependency-ordered. Landing
all `CN-*` phases realizes the design.

This skeleton acknowledges existing landed substrate. The first three
priority tools from the design (`bbox_code_query`,
`bbox_code_node_describe`, `bbox_code_symbols`) already exist as
syntax-only surfaces at `src/code_nav/mod.rs` and
`src/tools/code_nav.rs`. The work below hardens those surfaces against
the design's stated invariants, adds `symbol_kind` to the indexed data
model, layers `mode="indexed"` over `bbox_code_symbols`, and adds
`bbox_code_refs` as the new syntax-reference extractor.

---

## CN-0 — current substrate audit

**Scope.** Single one-off catalogue of what's already landed. Not a
code change; this entry exists so reviewers and future agents do not
re-derive the gap analysis.

**Landed today.**

- `src/code_nav/mod.rs` — `code_query`, `code_node_describe`,
  `code_symbols` with a shared `parse_code_nav_source` helper and a
  `CODE_SYMBOL_SKIP_DIRS` exclusion list.
- `src/tools/code_nav.rs` — `bbox_code_query`,
  `bbox_code_node_describe`, `bbox_code_symbols` `#[tool]`
  handlers.
- `src/tool_docs.rs` — stanzas for all three tools, including
  handoff guidance to `bbox_refactor_status` /
  `bbox_refactor_project_refs`.
- `src/system_memory/refactor.md` — cross-language guidance points
  at these tools as the syntax-locator surface before refactor
  planning.
- `bbox_code_symbols` ships as **live-only** today: it walks the
  project tree, parses every supported source file via the existing
  `crate::refactor::status` pipeline, and filters by
  `item_kinds`, `query`, `path_contains`, and `languages`. There is
  no `mode` parameter; no tantivy lookup path; no `symbol_kind`
  persistence.

**Open gaps versus the design doc.**

1. `bbox_code_symbols` has **no `mode="indexed"` lane**. The design
   explicitly calls for both `indexed` (default once `symbol_kind`
   lands) and `live` (capped) modes. Today's behaviour is
   `mode="live"` implicitly.
2. `bbox_code_symbols` does not enforce a **registered-project gate**
   nor a **per-file size cap**. It accepts any `project_dir` that is a
   real directory and parses files of arbitrary size. The design
   requires both gates for the live path.
3. **`symbol_kind` is not persisted.** `SymbolSpec` (in
   `src/chunker/code.rs`), `Chunk` (in `src/chunker/mod.rs`), the
   tantivy schema in `src/index/mod.rs`, the project-file ref
   pipeline (`src/refactor/...` and `src/index/project_files.rs`),
   and the embedding source-doc projection do not carry the
   tree-sitter node kind.
4. `bbox_code_refs` does not exist.
5. `bbox_code_query` and `bbox_code_node_describe` are
   single-file-only as the design requires, but neither tool
   currently rejects oversized inputs with a typed parse-report
   error — they read the whole file into memory before
   parsing. This is fine for the typical case; the cap is a
   defence-in-depth follow-up.
6. `CodeNodeDescribeResponse` does not currently carry a
   top-level `semantic_status` field (`src/code_nav/mod.rs`
   around the `CodeNodeDescribeResponse` struct), while
   `CodeQueryResponse` and `CodeSymbolSearchResponse` both do.
   The design's syntax-vs-semantic labeling rule is therefore
   silently violated on the describe surface. CN-S2 fixes this.

**Realizes.** Section "Current State" of the design doc.

**Gates.** None — informational phase.

**Follow-ups.** None.

---

## Substrate phases — shared invariants

### CN-S1 — registered-project + size gates for code-nav

**Scope.** Make every code-nav tool that walks or parses files reject
inputs that exceed shared safety caps. The caps are shared across
`bbox_code_query`, `bbox_code_node_describe`, `bbox_code_symbols`, and
the future `bbox_code_refs`.

**Realizes.** Design Boundary section's invariant that "Any tool that
returns syntactic references must label them as syntax-derived" plus
Phase 1's "enforce file-size and registered-project gates for
multi-file work".

**Components.**

- New module-private constants in `src/code_nav/mod.rs`:
  `MAX_CODE_NAV_FILE_BYTES` (default 2 MiB), `MAX_CODE_NAV_SCANNED_FILES`
  (default 5000, matches current `file_limit` cap).
- `parse_code_nav_source` rejects sources larger than the cap with a
  typed `parse_report` whose `errors` field carries
  `file_too_large_for_code_nav` and the byte count. Return value
  stays a `Result<CodeNavParsedSource>` for non-cap failures; the cap
  is reported as a structured error response by each tool wrapper,
  not an `anyhow!` bail, so the agent can keep reasoning.
- `code_symbols` consults the registered project list via
  `self.state.projects.read().list()` on the daemon handler side
  (mirrors `src/tools/refactor.rs:55,69`), so the registry source
  is `crate::projects::ProjectRegistry`. The free function in
  `src/code_nav/mod.rs` gains a `registered_projects: &[ProjectRecord]`
  parameter; the `#[tool]` handler in `src/tools/code_nav.rs`
  threads the read-lock snapshot through. `code_symbols` rejects
  `project_dir` paths that are not a registered project root *or* a
  descendant of one. The error response includes
  `registered_projects: [...]` to make recovery cheap.
- Size cap re-checked before `fs::read_to_string` so very large
  files are not loaded into memory: stat first, reject second.
- Unit tests covering (a) oversize file rejection on each tool, (b)
  unregistered `project_dir` rejection on `code_symbols`, (c)
  registered-project descendant acceptance.

**Gates.**

- `bbox_code_symbols` on an unregistered ad-hoc directory returns
  `status: "error"` with a fixable suggestion, not `status: "ok"`.
- `bbox_code_query` on a 5 MiB file returns a typed error response
  with `file_too_large_for_code_nav`, not an OOM or a 30-second
  parse.
- Live-path scan terminates with `truncated: true` and a populated
  `file_limit_hit` flag once `MAX_CODE_NAV_SCANNED_FILES` is reached
  (already partially implemented — this phase wires the constant
  through).
- Existing tests continue to pass.

**Follow-ups.**

- Per-language size caps if any one grammar becomes pathological
  (defer until observed).
- Optional structured warning when caps are *near* the limit so
  callers can chunk the request voluntarily.

---

### CN-S2 — `semantic_status` invariant test

**Scope.** Lock the design's syntax-vs-semantic labeling rule with a
unit test, not just documentation.

**Realizes.** "Any tool that returns syntactic references must label
them as syntax-derived or heuristic."

**Components.**

- A unit test in `src/code_nav/tests.rs` (or a new
  `tests/semantic_status.rs` if the file becomes crowded) that
  invokes each code-nav tool against a minimal Rust fixture and
  asserts the top-level `semantic_status` field is `"syntax_only"`
  (or `"structural_only"` once such a kind exists).
- Add a Rust constant `CODE_NAV_SEMANTIC_STATUS_SYNTAX_ONLY: &str = "syntax_only"`
  and replace every literal `"syntax_only".to_string()` with it.
- `bbox_code_refs` (CN-T1) MUST consume the same constant.

**Gates.**

- Test fails on any code-nav tool that returns
  `semantic_status: "semantic"` or omits the field.
- `grep "syntax_only"` in `src/code_nav/` returns the constant
  definition and the tests only — no scattered string literals.

**Follow-ups.**

- Once heuristic-confidence edges are surfaced from
  `bbox_code_refs`, extend the assertion to allow
  `edge_confidence: "heuristic"` on per-record entries while keeping
  the top-level field `syntax_only`.

---

## Data-model phases — `symbol_kind` persistence

These phases land the schema/data-model change required to expose
`mode="indexed"` on `bbox_code_symbols` with kind filtering. They are
sequenced so each phase is independently reviewable and
roll-forward-only: every phase preserves backward compatibility with
older indices until CN-D5 bumps the schema version and forces a
reindex.

### CN-D1 — `SymbolSpec.kind` + `parent_kind`

**Scope.** Carry both the tree-sitter node kind AND the immediate
named-parent kind from chunker symbol extraction into the
`SymbolSpec` value type. Parent kind is required because raw node
kind alone is ambiguous — a Rust `function_item` inside an
`impl_item` is what `refactor::status` synthesizes as
`"impl_method"`, and indexed records need to be able to derive that
synthetic vocabulary deterministically without re-parsing the file.

**Realizes.** Data Model Changes bullet 1.

**Components.**

- Add to `SymbolSpec` in `src/chunker/code.rs`:
  - `pub kind: String` — raw tree-sitter node kind.
  - `pub parent_kind: Option<String>` — kind of the nearest
    enclosing **symbol-producing** ancestor (i.e. the most recent
    entry pushed onto the `parents` stack in `collect_ast_symbols`),
    or `None` at file top level. **Not** the immediate tree-sitter
    `node.parent().kind()` — that would frequently be
    `declaration_list` / `block` / similar wrapper kinds, which
    carry no synthesis signal. Empty / unnamed AST parents are
    skipped during the walk; the field captures the kind of the
    containing symbol, not the raw AST shape.
- `collect_ast_symbols` threads a `parent_kind: Option<&str>`
  alongside the existing `parents: &mut Vec<String>` it already
  threads, and populates both fields per the design's
  "must be the tree-sitter node kind" rule. For Rust:
  - Top-level `function_item` → `kind="function_item"`,
    `parent_kind=None`.
  - `function_item` inside `impl_item` → `kind="function_item"`,
    `parent_kind=Some("impl_item")`.
- Rust `impl_item` keeps `kind="impl_item"`. Elixir keeps
  `kind="call"` (raw tree-sitter), with the
  defmodule/def/defp/defmacro distinction deferred to a
  separate optional display field per the design's "must be the
  tree-sitter node kind" rule.
- `collect_structure_item` (the `tree_sitter_language_pack`
  fallback) populates `kind` from the `StructureItem` shape and
  `parent_kind` from the immediate parent's kind on the walk
  stack — best-effort mapping documented in code comments;
  falls back to `"unknown"` when no kind is available.
- Unit tests in `src/chunker/code.rs` covering at least Rust,
  Java, Python, TypeScript, and Elixir fixtures. Assertions:
  - `SymbolSpec.kind` matches the tree-sitter grammar's
    documented node kind.
  - For Rust, a method declared inside `impl_item` has
    `parent_kind = Some("impl_item")`.

**Gates.**

- No public-API consumer of `SymbolSpec` regresses (this struct is
  module-private today — confirm at implementation time and keep it
  that way).
- All chunker tests pass; new tests cover every supported language
  the design's Acceptance Criteria call out (Rust, JS/TS, Python,
  Java).

**Follow-ups.**

- Document in `sm-refactor` the canonical kind strings per
  language so agents do not memorise grammar trivia.

---

### CN-D2 — `Chunk.symbol_kind`, `parent_kind`, and line ranges

**Scope.** Plumb the kind metadata AND line ranges through the
chunk record so downstream consumers (index, refs, embeddings) see
them without re-opening source files.

**Realizes.** Data Model Changes bullet 2; also makes CN-D3's
indexed line-range fields populatable.

**Components.**

- Add to `Chunk` in `src/chunker/mod.rs`, after `symbol_exact`:
  - `pub symbol_kind: Option<String>` — copied from
    `SymbolSpec.kind`.
  - `pub parent_kind: Option<String>` — copied from
    `SymbolSpec.parent_kind`. Optional today, but required for
    deterministic indexed `refactor_kind` derivation in CN-T2.
  - `pub line_start: Option<u32>` — 1-based line of `byte_start`,
    derived at chunk build time from the same source buffer the
    chunker already holds. `None` for non-line-oriented sources.
  - `pub line_end: Option<u32>` — 1-based line of `byte_end`.
- `chunks_from_symbols` writes all four fields when the chunk is
  built from a `SymbolSpec`. Line conversion uses the existing
  `line_col(...)`-style helper or a fresh `byte_to_line(source, byte)`
  in `src/chunker/code.rs`.
- Update `placeholder_chunk` / `Chunk::default`-style initialisers
  to default the new fields to `None`. Non-code chunkers
  (`markdown.rs`, `text.rs`) leave them `None`.
- All `Chunk` construction sites in `src/index/` and `src/refactor/`
  that build chunks from scratch (e.g. test fixtures, agentic-corpus
  build helpers) get the new fields; default to `None` when no
  metadata is known.

**Gates.**

- `cargo build` and `cargo test --bin blackboxd` pass.
- A unit test asserts `line_start <= line_end` for any chunk
  whose `byte_start <= byte_end`.
- Existing index round-trip tests still match — the new fields
  are additive.

**Follow-ups.**

- None — pure plumbing phase.

---

### CN-D3 — tantivy `symbol_kind` field + queryable extras + schema bump

**Scope.** Add `symbol_kind` to the tantivy schema **and** the
extra stored fields the indexed `code_symbols` lane needs
(`project_id`, `byte_end`, `line_start`, `line_end`), **and** bump
the schema version in the same phase. Splitting the field-write
from the version bump is unsafe: `TranscriptIndex::open_or_create`
calls `reset_index_on_schema_mismatch` before opening
(`src/index/mod.rs:149`), and the mismatch check compares only
`schema_version.txt` against `INDEX_SCHEMA_VERSION`
(`src/index/mod.rs:584`). Writing new fields with an unchanged
marker means a daemon binary with the new `FieldHandles` will open
an old tantivy directory whose schema does not contain those
fields — read-back panics or quietly returns nothing.

**Realizes.** Data Model Changes bullets 3 and 6 — merged because
mismatch detection forces them to land together.

**Components.**

- Add to the schema struct in `src/index/mod.rs`:
  - `pub symbol_kind: Field` — `STRING | STORED` (exact-token
    lookup). Raw tree-sitter node kind.
  - `pub parent_kind: Field` — `STRING | STORED`. Required so
    indexed records can derive synthetic `refactor_kind` (e.g.
    Rust `impl_method = function_item + parent impl_item`)
    without re-parsing the source file. See CN-T2.
  - `pub project_id: Field` — `STRING | STORED`. Today's
    `project_file` docs include the canonical path and the exact
    `entity_id` but no queryable `project_id` token
    (`src/index/project_files.rs:114,127`); the indexed
    `code_symbols` lane needs a fast term filter, so this field
    is added now and populated from the project record.
  - `pub byte_end: Field` — `u64 STORED`. Current schema only
    stores `byte_offset` (`src/index/mod.rs:42,550`); the indexed
    lane needs both ends to return a `byte_range` tuple matching
    the live lane.
  - `pub line_start: Field`, `pub line_end: Field` — `u64 STORED`.
    Sourced from `Chunk.line_start` / `Chunk.line_end` added in
    CN-D2.
- Index path (chunk → tantivy doc) writes the new fields whenever
  the chunk supplies them. Code chunks built from a `SymbolSpec`
  always do; non-code chunks may leave the line and kind fields
  unset — readers MUST treat an absent (or zero) value as
  "unknown" and not surface it. The read-side helpers use
  `optional_u64` / `optional_text` exactly as the existing
  field reads do.
- Read path: extend the document-extraction helpers (the
  `optional_text(&doc, self.fields.symbol_*)` cluster around
  `src/index/mod.rs:385-389`) to surface the new fields on
  search-result rows.
- Bump `INDEX_SCHEMA_VERSION` from
  `agentic-corpus-g5-symbol-tokenized` to
  `agentic-corpus-g6-symbol-kind-and-ranges`. Suffix is
  descriptive, not load-bearing — exact name TBD at landing.
- `agentic-corpus-release-notes.md` entry describing the bump
  and the on-first-startup reindex cost.
- `bbox_hybrid_search` keeps its existing behaviour — no new
  query-side filter exposed in this phase. CN-T2 introduces the
  filter on the typed `bbox_code_symbols` surface, not on the
  generic search tool.

**Gates.**

- A daemon binary built from this phase, started against a
  populated pre-bump index, detects the version skew via
  `reset_index_on_schema_mismatch` and rebuilds cleanly.
- New unit test indexing one Rust + one Java chunk and asserting
  every new field round-trips.
- `bbox_stats` returns a sane doc count post-rebuild.
- No regression in `bbox_hybrid_search` quality on a stored
  fixture corpus (smoke test, not a full eval).

**Follow-ups.**

- Whether to also store the `byte_offset → line_offset`
  conversion at write time as a single string token for
  jump-to-line UIs. Defer until a real caller asks.

---

### CN-D4 — `bbox_refactor_project_refs` carries `symbol_kind`

**Scope.** Surface `symbol_kind` on every emitted project-ref
record. No embedding work in this phase — see CN-D5 for that path.

**Realizes.** Data Model Changes bullet 5 (project refs).

**Components.**

- `bbox_refactor_project_refs` response carries `symbol_kind` on
  each emitted ref. The field is optional in the JSON for backward
  compatibility (callers parsing the old shape continue to work);
  emitted whenever the underlying chunk has `symbol_kind = Some(_)`.
- Tool-doc entry for `bbox_refactor_project_refs` updated to
  mention the new field.
- Backward-compatibility test: a deserialiser that ignores
  unknown fields parses both old and new response shapes.

**Gates.**

- `bbox_refactor_project_refs` on a known Rust file returns
  records with `symbol_kind: "function_item"` /
  `"impl_item"` / `"struct_item"` as appropriate.

**Follow-ups.**

- None.

---

### CN-D5 — embedder text + content-hash invalidation (optional)

**Scope.** Make the embedder *actually* see `symbol_kind` and
recompute affected vectors. Required only if vector search
should benefit from the new field; pure indexed-symbol lookup
in CN-T2 does not depend on it. Spelled out because the design
doc bullet 4 ("embedding source docs that include `symbol_kind`")
is not realised by metadata alone given today's plumbing.

**Realizes.** Data Model Changes bullet 4.

**Components.**

- Today, `enqueue_project_file` (`src/embed_queue.rs:153`) builds
  the embedder request from `chunk.content` verbatim, and the
  dedupe key on the consumer side is `(entity_id, content_hash)`
  where `content_hash = chunk.chunk_hash` (`src/embed_queue.rs:163,
  src/embed/queue.rs:722`). Adding `symbol_kind` to chunk
  metadata does NOT change either the embed text or the dedupe
  key — so without an explicit invalidation move, no
  recomputation happens.
- Introduce a small `project_file_embed_text(chunk: &Chunk) -> String`
  builder in `src/embed_queue.rs` that emits, when chunk metadata
  is present, a header line of the form
  `// language: rust\n// symbol: foo::bar\n// kind: function_item\n`
  followed by `chunk.content`. Pure-content chunks (no symbol,
  no kind) keep the original `chunk.content` exactly to avoid
  unnecessary recomputation. Header format is stable and
  versioned (see below).
- `enqueue_project_file` rebuilds `chunk_hash` against the
  embedder text by piping it through the existing
  `content_hash(...)` helper (`src/embed_queue.rs:406`), keeping
  the chunk-internal `chunk.chunk_hash` (used by the tantivy
  doc id and provenance) untouched. The result is that the
  embedder sees the new text AND the dedupe key flips, forcing
  recomputation only for the affected entries. Two-key plumbing
  is the minimal change; alternative designs (a separate
  `embed_hash` field on `Chunk`) are out of scope for this
  phase.
- Header version constant (`PROJECT_FILE_EMBED_TEXT_V1` or
  similar) gates future format changes.
- `bbox_embed_status` documents the expected one-time recompute
  burst after this phase lands. No code change; release note
  only.

**Gates.**

- New unit test in `src/embed_queue.rs` showing
  `project_file_embed_text` is stable for a chunk with no
  metadata (== `chunk.content`) and changes when `symbol_kind` is
  present.
- New integration test showing the embedder dedupe key changes
  for a chunk whose `symbol_kind` becomes populated after a
  reindex.

**Follow-ups.**

- Add per-route opt-in if the recompute cost on a large host is
  severe (env var or config field). Defer until observed.
- Promote the embed-text builder to a trait if a second consumer
  needs to assemble similar metadata-augmented text.

---

## Tool phases — exposed surfaces

### CN-T1 — `bbox_code_refs`

**Scope.** Add the Priority 4 tool from the design doc: a per-file
syntax-reference extractor.

**Realizes.** Design Phase 4 plus the "No live syntax reference
extractor" gap in Real Gaps section 5.

**Components.**

- New module entry `src/code_nav/refs.rs` (or a `refs` submodule
  inside `mod.rs` if file size stays reasonable) implementing
  `code_refs(&CodeRefsParams) -> Result<String>`.
- New `#[tool]` handler `bbox_code_refs` added to
  `src/tools/code_nav.rs` (mirrors the existing three handlers),
  re-exporting `code_refs` from the `crate::code_nav` module.
- `mod` declaration / pub exports in `src/code_nav/mod.rs`
  expose `CodeRefsParams` and `code_refs`.
- `src/tool_docs.rs` stanza added (see CN-X1 for the full prose;
  this phase lands the minimum entry needed to satisfy the
  every-`#[tool]`-has-a-stanza compile-time assertion).
- Param struct mirrors the design signature exactly:
  - `file: String`
  - `project_dir: Option<String>`
  - `kind: CodeRefKind` enum with serde rename `"calls" | "imports" | "fields" | "identifiers" | "all"`
  - `query: Option<String>` — case-sensitive substring against
    the displayed name
  - `limit: Option<usize>` — default 200, cap 1000
- Response struct includes `status`, `path`, `language`,
  `kind_filter`, `matching_refs`, `returned_refs`, `truncated`,
  `parse_report`, `semantic_status: "syntax_only"`, and a
  `refs: Vec<CodeRefRecord>`.
- Each `CodeRefRecord`: `kind` (one of `"call" | "import" |
  "field" | "identifier"`), `name`, `node_kind`, `byte_range`,
  `line_range`, optional `containing_symbol` (resolved from the
  ancestor `SymbolSpec`-style walk), `edge_confidence: "heuristic"`,
  and a `handoff` block pointing at `bbox_refactor_status` and
  `bbox_inspect_entity` when the ref resolves to an indexed
  symbol.
- Extraction strategy:
  - Per-language tree-sitter S-expression query strings stored as
    Rust constants for Rust, Java, TypeScript, JavaScript, Python,
    Go (initial set). Each query yields the node-kind capture
    name (`@call`, `@import`, `@field`, `@identifier`).
  - Languages without a curated query fall back to a generic
    walker that emits `identifiers` only — `kind != "identifiers"`
    on an unsupported language returns
    `status: "unsupported_language"` with the supported list.
  - The walker stays well under the per-file cap from CN-S1.
- Optional "indexed graph hint" enrichment: when the extracted
  identifier exactly matches a known `symbol_exact` in the
  registered project, attach `indexed_symbol_ref:
  "symbol:<...>"` on the record with
  `edge_confidence: "heuristic"`. The lookup is best-effort —
  failures are silent. This stays a *hint*, not a binding claim,
  per the design's "graph hint with confidence, not as binding
  truth" rule.
- Unit tests covering: Rust calls in one fixture, Java imports
  in one fixture, TypeScript fields in one fixture, Python
  identifiers in one fixture, and one unsupported-language error
  path.

**Gates.**

- `bbox_code_refs(file=…, kind="all")` returns at least one
  record on a Rust fixture with three call sites.
- Every record has `semantic_status: "syntax_only"` (top-level)
  and `edge_confidence: "heuristic"` (per-record).
- Unsupported-language + non-`identifiers` kind returns the
  documented error response, not a panic.
- Truncation is honoured at `limit`.

**Follow-ups.**

- Multi-file batch variant (deferred — single-file is the V1
  contract).
- Type-aware reference resolution (out of scope — that is LSP
  territory, see design's Non-Tool section).

---

### CN-T2 — `bbox_code_symbols` `mode` parameter

**Scope.** Add `mode` to the symbol search params and implement the
indexed lane. Live behaviour stays available and reachable via
`mode="live"`.

**Realizes.** Design Phase 3 plus the "No ergonomic indexed symbol
search" gap in Real Gaps section 3.

**Dependencies.** CN-D1 through CN-D3 must be landed (the indexed
path needs `symbol_kind`, `project_id`, `byte_end`, `line_start`,
`line_end` in tantivy plus the schema-version bump). CN-D4 is
strictly required for nothing in this phase but is a natural
neighbour; CN-D5 is optional (only matters if the indexed lane
should benefit from kind-aware vector text).

**Components.**

- `CodeSymbolSearchParams` gains
  `mode: Option<CodeSymbolMode>` with default `indexed` after
  CN-D5, transitional default `live` if shipped before the
  data-model phases land (kept as a compile-time switch so the
  tool stays usable mid-migration).
- **Live vs indexed `kind` contract.** This is the
  load-bearing piece. Today's live lane sources `SyntaxItem.kind`
  from `refactor::status`, which surfaces mostly raw tree-sitter
  kinds (`refactor::status` uses `node.kind()` directly at
  `src/refactor/mod.rs:2595`-area for the generic path, and Java
  uses raw method/constructor kinds at
  `src/refactor/java.rs:74`-area) plus one known synthetic case:
  Rust `impl_method`, synthesised in `src/refactor/rust.rs:1809`-area
  for `function_item` nodes whose parent is `impl_item`. The
  proposed indexed lane sources `symbol_kind` from
  `Chunk.symbol_kind`, which CN-D1 pins to the raw tree-sitter
  node kind. The two views diverge ONLY at the synthetic cases.
  The contract for this phase is:
  - Indexed records carry `symbol_kind` (raw tree-sitter node
    kind, from CN-D3) and `parent_kind` (raw tree-sitter parent
    kind, from CN-D3). On the read path, the indexed lane
    derives `refactor_kind` deterministically from
    `(language, symbol_kind, parent_kind)` using the same
    synthesiser logic the live lane runs — extracted into a
    pure function `refactor_kind_for(language, symbol_kind,
    parent_kind: Option<&str>) -> String` shared by both
    `refactor::status` (where it lives today implicitly) and
    the indexed `code_symbols` lane. Both `symbol_kind` and
    `refactor_kind` end up on every indexed record. Today's
    synthesiser surface is small (`impl_method` is the only
    documented synthesis), so the function stays small; if
    future languages add synthesised kinds, the function is
    the single place to update.
  - Live records carry `refactor_kind` natively (unchanged
    today as the existing `kind` field) and gain `symbol_kind`
    via the same shared function, reverse-projected from the
    syntax item's node kind.
  - The `kind` filter on `CodeSymbolSearchParams.item_kinds`
    accepts BOTH vocabularies and matches against either field
    after canonicalisation.
  - Tool-doc + `sm-refactor` text spells out the dual
    vocabulary so agents do not guess.
- Indexed lane:
  - Resolves `project_dir` to a project_id on the handler side
    in `src/tools/code_nav.rs` by taking
    `self.state.projects.read().list()` (a clone-snapshot per
    `ProjectRegistry::list` at `src/projects.rs:300`) and
    matching the canonicalised `project_dir` against
    `ProjectRecord.canonical_path` (field at
    `src/projects.rs:57`). No new `lookup_by_dir` helper is
    introduced; the existing `list()` + linear scan is fine for
    the project-count cardinality the daemon sees. Refactor
    apply uses the same pattern (`src/tools/refactor.rs:55,69`).
    If profiling later shows the linear scan is hot, a typed
    helper can be added — out of scope here.
  - Builds a tantivy `BooleanQuery` over the `project_file`
    document type with that project_id, optional `language`
    filter, `symbol`/`symbol_exact` substring or term match,
    optional `path` substring (via `path_tokens`), and
    `symbol_kind` term filter when supplied. Filter uses the
    `project_id`/`parent_kind`/`byte_end`/`line_start`/`line_end`
    fields added in CN-D3 — without those, this lane cannot
    land.
  - Reads `byte_range`, `line_range`, `chunk_hash`, and ref
    components from the stored fields; reconstructs the
    `handoff` block from `path_tokens` + `chunk_hash` exactly as
    the live path does, so callers see a stable shape across
    modes.
  - Returns the same `CodeSymbolSearchResponse`, with one extra
    field `mode: "indexed"` so agents can see which lane
    answered.
- Live lane stays in place. The shared response type gets
  `mode: String` populated on both paths.
- The live lane begins rejecting `item_kinds` when `mode="live"`
  is explicitly requested only if CN-S1 has landed (kind
  filtering remains supported on live — it parses on the fly).
  The indexed lane rejects unknown kinds with a fixable error
  pointing at the canonical kind list from the renderer.
- Param-doc text updated; tool-docs stanza updated to call out
  the new `mode` field and the recommended default.

**Gates.**

- `mode="indexed"` on a project with no `symbol_kind` index
  returns `status: "needs_reindex"` with a recovery hint, not
  empty results.
- `mode="indexed"` on a project WITH a fresh index returns the
  same logical items as `mode="live"` for the same query.
  Equivalence is defined over the canonicalised set of
  `(file, byte_start, byte_end, symbol_kind, refactor_kind, name)`
  tuples, modulo files filtered by `CODE_SYMBOL_SKIP_DIRS`. Both
  lanes compute `refactor_kind` via the same
  `refactor_kind_for(language, symbol_kind, parent_kind)`
  function, so the tuple comparison is a direct set equality —
  not a mapping-table reconciliation. A unit test enumerates
  known synthesis cases (Rust `function_item` + `impl_item`
  parent ↔ `impl_method`) and runs the equivalence check on a
  fixture project containing each case.

**Follow-ups.**

- Optional `mode="auto"` that picks indexed when available and
  falls back to live — deferred until usage shows the choice is
  load-bearing.
- Cross-project search (multiple `project_dir` values). The
  current single-project contract is intentional; revisit if a
  real workflow needs it.

---

### CN-T3 — `bbox_code_query` / `bbox_code_node_describe` hardening

**Scope.** Apply the size/registered-project gates from CN-S1 to the
two single-file tools and add the missing tests from the design's
Acceptance Criteria.

**Realizes.** Design's Acceptance Criteria bullet "Tests cover at
least Rust, JavaScript/TypeScript, Python, and Java query or
node-describe behavior, plus one unsupported-language error path."

**Components.**

- `bbox_code_query` returns a typed `parse_report.errors` entry
  on oversized input (uses CN-S1's machinery).
- `bbox_code_node_describe` likewise.
- Single-file tools accept an absolute file path OR a
  project-relative path resolved via `project_dir`. When
  `project_dir` is supplied, CN-S1's registered-project gate
  applies; when omitted, the tool falls back to the existing
  behaviour but emits a `warning: "no_project_context"` field
  so agents notice the soft mode.
- Test fixtures under `src/code_nav/tests/fixtures/` for Rust,
  Java, JavaScript/TypeScript, Python, and one unsupported
  extension (e.g. `.foo`). Each fixture exercises at least one
  capture on `bbox_code_query` and one node-describe call.
- Existing tests stay green; new tests added.

**Gates.**

- All five language tests pass.
- Unsupported-language path returns a structured error.
- Oversized-file path returns a structured error with byte
  count, not a panic.

**Follow-ups.**

- Optional structured `query_diagnostics` output from
  `bbox_code_query` (the design notes this only as a "good
  observability" idea, not a contract).

---

## Cross-cutting phases

### CN-X1 — tool-docs + system-memory rendering

**Scope.** Re-author the tool-docs stanzas and the
`sm-refactor` / `sm-refactor-rust` / `sm-refactor-java` runbooks so
the post-landing behaviour is documented in the right voice — index
vs live, `symbol_kind` filtering, `bbox_code_refs` as the new
syntax-ref tool.

**Realizes.** Design's first Acceptance Criterion ("Tool docs
explain syntax-only vs graph vs LSP semantics.") and last
("Generic structural declaration rewrites are not named like
semantic rename.").

**Components.**

- `src/tool_docs.rs` edits for `bbox_code_query`,
  `bbox_code_node_describe`, `bbox_code_symbols`, plus a new
  stanza for `bbox_code_refs`. The compile-time unit test in
  `src/tool_docs.rs` that asserts every `#[tool]`-registered
  name has a stanza fails the build if `bbox_code_refs`'s
  stanza is missing — that test IS this phase's primary gate.
- `src/system_memory/refactor.md` gains a paragraph on
  `bbox_code_refs` and the
  `bbox_code_symbols(mode="indexed", kind=…)` workflow.
- `src/system_memory/refactor-java.md` and any future
  language-scoped runbooks get the canonical kind strings for
  their grammar.
- The `sync_into_knowledge` startup path re-emits
  `bb-tool-reference` with the new stanzas.

**Gates.**

- `cargo test --bin blackboxd` passes (covers the
  every-tool-has-a-stanza assertion).
- `bbox_knowledge(query="bb-tool-reference")` after a daemon
  restart returns the new content.

**Follow-ups.**

- None.

---

### CN-X2 — design-doc Acceptance Criteria audit

**Scope.** One-line phase: re-read `code-nav-symbolic-exploration.md`
Acceptance Criteria after CN-T3 and CN-X1 land and tick each one
off against landed behaviour. Mechanical, but it's the named close.

**Realizes.** All six Acceptance Criteria.

**Components.**

- A short audit note appended to the design doc (or to a
  release-notes entry) confirming:
  1. Tool docs explain syntax-only vs graph vs LSP semantics.
  2. Query and node-describe tools return bounded responses
     with parse diagnostics.
  3. Kind-filtered indexed symbol search is not exposed until
     `symbol_kind` exists (CN-D3).
  4. Existing graph navigation remains the preferred route for
     indexed callers, callees, and evidence bundles.
  5. Generic structural declaration rewrites are not named like
     semantic rename.
  6. Tests cover at least Rust, JavaScript/TypeScript, Python,
     and Java query or node-describe behaviour, plus one
     unsupported-language error path (CN-T3).

**Gates.**

- Audit note exists and is checked into the design folder.
- Design doc status moves from `proposal` to `landed` (or the
  equivalent terminal state).

**Follow-ups.**

- Move the design doc out of `proposed/` into the archived
  location once all phases have landed.

---

## Dependency graph

```
CN-0  (audit, informational)
CN-S1 ───► CN-T1
CN-S2 ───► CN-T1
CN-D1 ─► CN-D2 ─► CN-D3 ─► CN-T2
                  └─► CN-D4 (parallel with CN-T2 once D3 lands)
                  └─► CN-D5 (optional; required only if
                              vector search must benefit from
                              kind-aware embed text)
CN-S1 ───► CN-T2
CN-T1, CN-T2, CN-T3 ───► CN-X1 ───► CN-X2
CN-S1 ───► CN-T3
```

Phases inside a dependency chain are strictly ordered; the chains
can land in parallel. CN-X1 / CN-X2 close the loop and depend on
every tool phase.

Note the merge of the original CN-D3 ("add field") and CN-D5
("bump version") into a single CN-D3 phase. Splitting them is
unsafe given `reset_index_on_schema_mismatch` — see CN-D3's Scope
note for the mechanism.

---

## Out of scope

The following are explicitly NOT part of this skeleton, matching the
design doc's Non-Tool sections:

- A generic `bbox_refactor_plan(kind="rename_symbol")` surface. Use
  `bbox_refactor_plan(kind="rust_lsp_rename")` for binding-aware
  rename. A future `rewrite_declaration_name_in_file` belongs in
  the refactor plan family, not in code-nav.
- LSP-backed find-references. `bbox_code_refs` is intentionally
  syntax-only.
- Multi-file `bbox_code_query`. Design defers this to a future
  phase behind a registered-project + size gate; not implemented
  here.
- Macro expansion, type inference, or any answer that requires a
  compiler. The graph + LSP surfaces own that authority.

---

## CN-X2 audit — Acceptance Criteria status (2026-05-12)

The design-doc Acceptance Criteria from
`design/proposed/code-nav-symbolic-exploration.md`:

1. **Tool docs explain syntax-only vs graph vs LSP semantics.**
   ✓ `src/tool_docs.rs` stanzas for `bbox_code_symbols`,
   `bbox_code_query`, `bbox_code_node_describe`, and
   `bbox_code_refs` each name the syntax-only label and the
   handoff to LSP / refactor plan / graph surfaces. The
   dual-vocabulary (refactor synthetic vs raw tree-sitter)
   discussion is specific to `bbox_code_symbols` (the only tool
   that surfaces both kinds on the same record) and lives in
   that stanza plus `sm-refactor`. `src/system_memory/refactor.md`
   carries the longer discussion + error-shape reference.
   Re-rendered into provider files on daemon startup via the
   `bb-tool-reference` mechanism.
   `tool_docs::tests::description_summary_parity` enforces that
   the `#[tool(description=...)]` strings in handlers match the
   `ToolDoc.summary` strings in `tool_docs.rs` byte-for-byte.

2. **Query and node-describe tools return bounded responses with
   parse diagnostics.**
   ✓ Both honour CN-S1's `MAX_CODE_NAV_FILE_BYTES` cap (typed
   `file_too_large_for_code_nav` error response with `file_bytes`
   + `max_bytes`). Both carry `parse_report` on every successful
   response.

3. **Kind-filtered indexed symbol search is not exposed until
   `symbol_kind` exists.**
   ✓ `bbox_code_symbols(mode="indexed")` reads stored
   `symbol_kind` from tantivy — the field landed in CN-D3 along
   with the schema-version bump
   (`agentic-corpus-g6-symbol-kind-and-ranges`). Records that
   predate the bump are skipped by the indexed lane; the live
   lane is the documented fallback.

4. **Existing graph navigation remains the preferred route for
   indexed callers, callees, and evidence bundles.**
   ✓ Every code-nav tool's success records carry a `handoff` block
   pointing at `bbox_refactor_status` / `bbox_refactor_project_refs`
   with pre-filled argument shapes (`bbox_code_symbols` items,
   `bbox_code_query` captures, `bbox_code_node_describe` response
   root, and — after CN-T1's closing fix — `bbox_code_refs`
   records). `sm-refactor` explicitly directs binding-authority
   questions to `bbox_inspect_entity` / `bbox_find_paths` /
   `bbox_bundle_evidence`. `bbox_code_refs` records additionally
   carry `edge_confidence: "heuristic"` so callers cannot mistake
   syntax matches for graph-resolved references.

5. **Generic structural declaration rewrites are not named like
   semantic rename.**
   ✓ No new tool emits a generic `rename_symbol` plan kind.
   `bbox_refactor_plan(kind="rust_lsp_rename")` remains the
   binding-aware path; a future structural declaration-only
   rewrite would land under the refactor-plan family with an
   explicit `structural_only` semantic_status.

6. **Tests cover at least Rust, JavaScript/TypeScript, Python,
   and Java query or node-describe behaviour, plus one
   unsupported-language error path.**
   ✓ `src/code_nav/tests.rs` covers:
   - Rust: `test_code_query_rust`,
     `test_code_query_handoff_maps_rust_impl_method_to_refactor_status_kind`,
     `code_symbols_live_lane_populates_symbol_kind_and_parent_kind`,
     `code_refs_rust_calls_resolve_containing_symbol`.
   - JavaScript: `test_code_query_javascript`.
   - TypeScript: `test_code_query_typescript`,
     `test_code_node_describe_typescript` (CN-T3 landed these
     explicitly because the existing JS tests didn't exercise
     the TS grammar).
   - Python: `test_code_node_describe_python`,
     `code_refs_python_identifiers_with_query_filter`.
   - Java: `test_code_query_java`,
     `test_code_query_handoff_suggests_refactor_status_for_java_method`,
     `test_code_symbols_finds_java_method_line_ranges_without_rg`,
     `code_refs_java_imports`.
   - Unsupported-language paths: `test_unsupported_language_error`
     (existing) plus
     `code_refs_unsupported_language_typed_error_for_non_identifier_kind`
     (typed error from `bbox_code_refs`).

### Phases landed

CN-0 (audit), CN-S1 (gates), CN-S2 (semantic_status invariant),
CN-D1 → CN-D4 (data model + schema bump; CN-D5 deferred as
explicit optional), CN-T1 (`bbox_code_refs`), CN-T2 (mode +
indexed lane), CN-T3 (TS coverage), CN-X1 (this doc set), CN-X2
(this audit).

### Phases deferred (known limitations, future work)

- **CN-D5 — embed-text builder + content-hash invalidation.**
  Optional per the impl doc; the indexed code_symbols lane does
  not depend on it. Vector search will not benefit from
  kind-aware embed text until this lands.
- **Indexed graph hints on `bbox_code_refs`.** The impl doc lists
  this as an optional V1 enrichment ("when extracted identifier
  matches a known `symbol_exact`, attach `indexed_symbol_ref`").
  Deferred — the V1 surface stays strictly syntax-only.
- **Single-file tool registered-project gate.** CN-T3 noted the
  trade-off: adding the gate would force every existing test
  through `registered_for(&dir)` for marginal value over the
  CN-S1 file-size gate. Future work if a concrete misuse
  emerges.
- **`chunker::code::is_symbol_node` audit for `const_item` /
  `static_item` / `macro_definition`.** Codex round-3 review
  flagged that code_nav surfaces these as refactor item kinds
  but the symbol walker may not emit them. Audit + extend the
  symbol-node set as part of the next chunker iteration; not a
  blocker for the current Acceptance Criteria but a coverage
  gap for some Rust workloads.

### Codex review history

Three rounds against the durable code-nav review bro (session
`019e1d18-9fc9-7823-9bc1-d36219f12b61`):

- Round 1 (CN-S2..CN-T2): REVISE → 4 must-fix items (live-lane
  size gate hole, indexed kind filter copy-paste bug, dishonest
  truncation, anyhow-bail on invalid mode) — addressed in
  commit `b7d32d3`.
- Round 2 (post-fixes): REVISE → 3 must-fix items
  (scan_cap_reached semantics conflating fast-path and real
  cap, missing doc-comment entry, missing language guard on
  synthesis decomposition) — addressed in commit `d9a3376`.
- Round 3 (post-fixes): APPROVE_WITH_NITS. One deferred
  coverage nit (`const_item`/`static_item`/`macro_definition`)
  tracked above.

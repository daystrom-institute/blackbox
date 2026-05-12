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
- `code_symbols` consults `bbox_project_register` state via
  `crate::orchestration` (or whichever module owns the registered
  project list — confirm at implementation time) and rejects
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

### CN-D1 — `SymbolSpec.kind` field

**Scope.** Carry the tree-sitter node kind from chunker symbol
extraction into the `SymbolSpec` value type.

**Realizes.** Data Model Changes bullet 1.

**Components.**

- Add `pub kind: String` to `SymbolSpec` in `src/chunker/code.rs`.
- `collect_ast_symbols` populates `kind = node.kind().to_string()`
  for every emitted symbol. For Rust `impl_item` (custom display
  path) the kind stays `"impl_item"`; for Elixir `call` symbols
  promoted to defmodule/def/defp/defmacro, the kind is the matched
  marker (e.g. `"defmodule"`), not the parent grammar's `"call"` —
  this gives agents the meaningful name without leaking grammar
  quirks.
- `collect_structure_item` (the `tree_sitter_language_pack`
  fallback) populates `kind` from the
  `StructureItem` shape — best-effort mapping documented in code
  comments; falls back to `"unknown"` when no kind is available.
- Unit tests in `src/chunker/code.rs` covering at least Rust,
  Java, Python, TypeScript, and Elixir fixtures: assert the
  emitted `SymbolSpec.kind` matches the tree-sitter grammar's
  documented node kind.

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

### CN-D2 — `Chunk.symbol_kind` field

**Scope.** Plumb `symbol_kind` through the chunk record so
downstream consumers (index, refs, embeddings) see it.

**Realizes.** Data Model Changes bullet 2.

**Components.**

- Add `pub symbol_kind: Option<String>` to `Chunk` in
  `src/chunker/mod.rs`, after `symbol_exact`.
- `chunks_from_symbols` writes `chunk.symbol_kind = Some(spec.kind)`
  alongside `chunk.symbol` and `chunk.symbol_exact`.
- Update `placeholder_chunk` / `Chunk::default`-style initializers
  to default the new field to `None`. Non-code chunkers
  (`markdown.rs`, `text.rs`) leave it `None`.
- All `Chunk` construction sites in `src/index/` and `src/refactor/`
  that build chunks from scratch (e.g. test fixtures, agentic-corpus
  build helpers) get the new field; default to `None` when no
  kind is known.

**Gates.**

- `cargo build` and `cargo test --bin blackboxd` pass.
- Existing index round-trip tests still match — the new field is
  additive.

**Follow-ups.**

- None — pure plumbing phase.

---

### CN-D3 — tantivy `symbol_kind` field

**Scope.** Store and index `symbol_kind` in the tantivy schema so
`mode="indexed"` filtering does not require post-filtering.

**Realizes.** Data Model Changes bullet 3.

**Components.**

- Add `pub symbol_kind: Field` to the schema struct in
  `src/index/mod.rs`, alongside `symbol` and `symbol_exact`.
- Register the field with `builder.add_text_field("symbol_kind", STRING | STORED)`
  in the schema builder. STRING tokenization (not TEXT) — kinds are
  exact lookup tokens.
- Index path (chunk → tantivy doc) writes `symbol_kind` when
  `Chunk.symbol_kind` is `Some`.
- Read path: extend the document-extraction helpers (the
  `optional_text(&doc, self.fields.symbol_*)` cluster around
  line 388 of `src/index/mod.rs`) to surface `symbol_kind` on
  search-result rows.
- `bbox_hybrid_search` keeps its existing behaviour — no new
  query-side filter exposed in this phase. CN-T2 introduces the
  filter on the typed `bbox_code_symbols` surface, not on the
  generic search tool.

**Gates.**

- Schema builds; no panic on existing indices because the field is
  additive and STORED.
- New unit test indexing one Rust + one Java chunk and asserting
  the stored field round-trips its kind string.

**Follow-ups.**

- Whether to ALSO add `symbol_kind` to the embedding source
  projection (current source docs include `symbol` and
  `symbol_exact`). Default: yes — see CN-D4. Tracked as a
  dependency, not a separate follow-up.

---

### CN-D4 — embedding source-doc + `project_refs` output

**Scope.** Make `symbol_kind` visible to embedding routes and to
`bbox_refactor_project_refs` callers without forcing a separate
fetch.

**Realizes.** Data Model Changes bullets 4 and 5.

**Components.**

- Embedding source-doc projection: extend the projector that
  builds the text the embedder sees for each `project_file` chunk
  to include the kind as a labeled segment (e.g.
  `"kind: function_item"`). Keep formatting stable — embedding
  recomputation is gated by the schema version bump in CN-D5, not
  by this prose.
- `bbox_refactor_project_refs` response carries `symbol_kind` on
  each emitted ref. The field is optional in the JSON for backward
  compatibility (callers parsing the old shape continue to work);
  emitted whenever the underlying chunk has `symbol_kind = Some(_)`.
- Tool-doc entry for `bbox_refactor_project_refs` updated to
  mention the new field.

**Gates.**

- `bbox_refactor_project_refs` on a known Rust file returns
  records with `symbol_kind: "function_item"` /
  `"impl_item"` / `"struct_item"` as appropriate.
- Embedding recomputation is NOT triggered by this phase on its
  own — CN-D5 owns the schema-version cut.
- Backward-compatibility test: a deserialiser that ignores
  unknown fields parses both old and new response shapes.

**Follow-ups.**

- None.

---

### CN-D5 — schema-version bump + reindex requirement

**Scope.** Cut the schema version so daemons rebuild indices that
predate `symbol_kind`. This phase is the last in the data-model
chain; landing it activates the indexed lane in CN-T2.

**Realizes.** Data Model Changes bullet 6.

**Components.**

- Bump `INDEX_SCHEMA_VERSION` in `src/index/mod.rs` from
  `agentic-corpus-g5-symbol-tokenized` to
  `agentic-corpus-g6-symbol-kind`. Exact name TBD at landing —
  the suffix is descriptive, not load-bearing.
- Existing schema-mismatch path in the index bootstrap deletes
  and rebuilds the tantivy directory; verify on a populated test
  fixture that startup detects the version skew and rebuilds
  cleanly.
- `agentic-corpus-release-notes.md` entry describing the bump,
  the new `symbol_kind` field, and the on-first-startup reindex
  cost.
- `bbox_embed_status` recheck: vector routes recompute against
  the new source docs once they exist. No code change needed —
  document the operator observation.

**Gates.**

- Existing test fixture indices get rebuilt on first daemon start
  after the bump.
- `bbox_stats` returns a sane doc count post-rebuild.
- No regression in `bbox_hybrid_search` quality on a stored
  fixture corpus (smoke test, not a full eval).

**Follow-ups.**

- If the embedding cost on a large host is severe, gate the
  embed recomputation behind an opt-in env var. Defer until
  observed.

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

**Dependencies.** CN-D1 through CN-D5 must all be landed — the
indexed path needs `symbol_kind` in tantivy.

**Components.**

- `CodeSymbolSearchParams` gains
  `mode: Option<CodeSymbolMode>` with default `indexed` after
  CN-D5, transitional default `live` if shipped before the
  data-model phases land (kept as a compile-time switch so the
  tool stays usable mid-migration).
- Indexed lane:
  - Builds a tantivy `BooleanQuery` over the `project_file`
    document type with the requested `project_dir` (matched via
    `project_id` lookup), optional `language` filter,
    `symbol`/`symbol_exact` substring or term match, optional
    `path` substring (via `path_tokens`), and `symbol_kind` term
    filter when supplied.
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
  same items as `mode="live"` within an agreed equivalence
  (sort order may differ; the set of `(file, byte_range, kind,
  name)` tuples must match modulo files filtered by
  CODE_SYMBOL_SKIP_DIRS).
- Indexed lane is materially faster on a 1000-file fixture
  project — wall-clock smoke test, not a strict perf gate.

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

**Realizes.** All five Acceptance Criteria.

**Components.**

- A short audit note appended to the design doc (or to a
  release-notes entry) confirming:
  1. Tool docs explain syntax-only vs graph vs LSP semantics.
  2. Query and node-describe tools return bounded responses
     with parse diagnostics.
  3. Kind-filtered indexed symbol search is not exposed until
     `symbol_kind` exists (CN-D5).
  4. Existing graph navigation remains the preferred route for
     indexed callers, callees, and evidence bundles.
  5. Generic structural declaration rewrites are not named like
     semantic rename.

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
CN-D1 ─► CN-D2 ─► CN-D3 ─► CN-D4 ─► CN-D5 ─► CN-T2
CN-S1 ───► CN-T2
CN-T1, CN-T2, CN-T3 ───► CN-X1 ───► CN-X2
CN-S1 ───► CN-T3
```

Phases inside a dependency chain are strictly ordered; the chains
can land in parallel. CN-X1 / CN-X2 close the loop and depend on
every tool phase.

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

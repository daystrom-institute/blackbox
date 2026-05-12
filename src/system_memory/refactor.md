# Refactor Mechanization Catalog

Use this memory first when you know the task is restructuring but have not yet
picked a language-specific runbook.

## Support Matrix

Project-scoped symbol lookup uses `bbox_code_symbols`. Single-file syntax
exploration uses `bbox_code_node_describe` and `bbox_code_query`. Per-file
reference extraction (calls / imports / fields / identifiers) uses
`bbox_code_refs`. Refactor inventory uses `bbox_refactor_status` and is
available for any source file whose extension maps to a supported tree-sitter
parser in `CodeChunker`. The operational runbooks below cover the common
application languages; additional mapped languages are inspect-only unless a
newer language memory says otherwise.

`bbox_code_symbols`, `bbox_code_node_describe`, `bbox_code_query`, and
`bbox_code_refs` are syntax locators, not refactor planners. Use
`bbox_code_symbols` instead of shell `rg -n` when you need
method/function/type line numbers, candidate files for a symbol, or exact
refactorable item names. Use `bbox_code_node_describe` when you have a
line/column and need local grammar shape. Use `bbox_code_query` when the file
is known and you need a custom tree-sitter pattern. Use `bbox_code_refs` when
you want every call site / import / field access / identifier occurrence in
one file without re-parsing yourself. Every response carries
`semantic_status: "syntax_only"` and per-record `edge_confidence:
"heuristic"` (on `bbox_code_refs`) — these are syntactic captures, NOT
binding resolution. For binding authority, use LSP via `bbox_refactor_plan`
or graph traversal via `bbox_inspect_entity`. Responses include a `handoff`
block with suggested `bbox_refactor_status` and `bbox_refactor_project_refs`
calls; treat those as the bridge into the guarded refactor surfaces. Do not
turn raw query captures directly into edits unless the edit is a generic
literal plan such as `replace_text`.

### `bbox_code_symbols` modes — indexed (default) vs live

The tool ships two lanes, selected via the `mode` param:

- `mode="indexed"` (default when the daemon has a populated index): reads
  stored `project_file` docs from tantivy. No parse cost; works at any
  project size; carries `symbol_kind` (raw tree-sitter), `parent_kind`
  (nearest enclosing symbol kind), and `line_range` directly from stored
  fields. Truncation reports `truncation_reason: "scan_cap_reached"`
  when the tantivy match count exceeds the 5000-record scan cap on a
  post-filtered query — `matching_items` is a lower bound, not exact.
- `mode="live"`: walks the project tree and calls `bbox_refactor_status`
  per file. Slower but always reflects the on-disk state, even if the
  reindexer is behind. Honours `file_limit` and the
  `MAX_CODE_NAV_SCANNED_FILES` cap; oversized files are skipped with a
  typed per-file `file_too_large_for_code_nav` entry in `errors[]`.

### Dual `kind` vocabulary on `item_kinds`

`bbox_code_symbols(item_kinds=...)` accepts both vocabularies on the same
filter:

- Raw tree-sitter node kinds — `function_item`, `impl_item`,
  `struct_item`, `method_declaration`, `class_declaration`, etc. The
  same kinds emitted by `bbox_code_query` captures and stored in
  `symbol_kind`.
- Refactor synthetic kinds — `impl_method` (Rust `function_item` inside
  `impl_item`). The same kinds emitted by `bbox_refactor_status` and
  consumed by `bbox_refactor_plan`. Synthesised at read time via
  `refactor_kind_for(language, symbol_kind, parent_kind)`.

For Rust impl methods specifically, `item_kinds=["impl_method"]` and
`item_kinds=["function_item"]` both match — the synthetic form returns
ONLY impl methods, the raw form returns impl methods plus top-level
functions. Each record carries both `kind` (refactor synthetic) and
`symbol_kind` (raw) so the caller can dispatch on either.

`bbox_refactor_project_refs` is a grounding-only companion for metadata and
provenance repairs. It returns current
`project_file:<project>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>` refs for
one file using the same chunking/hash rules as the agentic corpus, and (post
CN-D4) carries the same `symbol_kind` / `parent_kind` / `line_start` /
`line_end` metadata as `bbox_code_symbols`. Use it before literal edits to
eval fixtures, citations, or expected refs; whole-file `sha256sum` is not a
valid substitute for a chunk hash.

### Error response shape

Code-nav tools return a typed `CodeNavErrorResponse` for recoverable failure
modes rather than bailing. The agent reads `code` for typed dispatch and
`suggestion` for the recovery call. Stable codes:

- `file_too_large_for_code_nav` — file exceeds 2 MiB cap. Includes
  `file_bytes` and `max_bytes`.
- `project_not_registered` — `project_dir` is not a registered root nor
  a descendant of one. Includes `registered_projects: [{canonical_path,
  project_id}, ...]`.
- `invalid_code_symbols_mode` — `mode` was something other than
  `"indexed"` / `"live"`.
- `unsupported_language_for_code_refs` — language has no curated
  reference query and the requested `kind` is not `"identifiers"`. Use
  `bbox_code_query` instead, or switch to `kind="identifiers"`.

Every error response carries `semantic_status: "syntax_only"` so the
labelling invariant holds across both ok and error paths.

- Rust: `rust`
- TypeScript / TSX: `typescript`
- JavaScript / JSX / MJS / CJS: `javascript`
- Python: `python`
- C#: `csharp`
- Java: `java`
- Go: `go`
- C: `c`
- C++: `cpp`
- Additional inspect-only parser mappings: Erlang, Elixir, Ruby, OCaml,
  Haskell, Swift, Kotlin, Scala, Lua, Bash, JSON, YAML, TOML, HTML, CSS, SQL

Writable structural plans are narrower:

- Generic:
  - `bbox_refactor_plan(kind="move_file")` moves a file from `source` to a
    missing `target`. Supported source files are syntax-validated at the
    destination path; unsupported file types still get hash, dirty-file,
    path-scope, target-exists, and rollback checks.
  - `bbox_refactor_plan(kind="replace_text")` replaces exact `old_text` with
    `new_text` in `source`. By default the old text must match exactly once;
    pass `replace_all=true` for every occurrence. This is a literal edit
    primitive with transaction/rollback guardrails, not a semantic refactor
    primitive. Treat it as grep-with-safety for hard-coded metadata, generated
    fixtures, or other already-grounded literals. Do not present it as symbolic
    rename, import repair, extraction, or move support.
  - `bbox_refactor_plan(kind="write_file")` replaces or creates `source` with
    complete `new_text`. Supported source files are parse-validated.
  - `bbox_refactor_plan(kind="ensure_toml_table")` ensures a top-level TOML
    table named by `toml_table` contains `toml_entries` values. This is for
    simple structured config edits such as adding `[lib]` to a manifest.
  Use `bbox_refactor_apply(confirm=true)` for one plan or
  `bbox_refactor_run(confirm=true)` when several primitive plans and command
  validations must succeed or rollback together.
  In `bbox_refactor_run` command steps, `command` is the executable only and
  arguments go in `args`: use `{"command":"cargo","args":["fmt"]}`, not
  `{"command":"cargo fmt"}`.
  Command steps have three failure modes via `on_failure` (RX-F2b):
  - `"required"` (default): exit-code != 0 rolls back and terminates.
  - `"optional"`: exit-code != 0 is logged and the run continues.
  - `"continue_for_repair"`: exit-code != 0 opens a repair obligation and
    continues. A later step (e.g. `rust_compile_fix_round`) must mark the
    obligation `Consumed` or `LeftOver`. Any obligation still `Open` at run
    end triggers rollback from the first soft-fail cursor. Consumed and
    LeftOver obligations remain live rollback anchors until terminal success
    — only reaching the end of the run without any failure releases them.
  The `on_failure` field supersedes the legacy `required: bool` when set.
  Canonical repair sequence: `[extract_plan, add_mod_decl, cargo_check
  (continue_for_repair, capture=rustc_json), rust_compile_fix_round, cargo_check
  (required=true), cargo_test (required=true)]`.
- Rust: `bbox_refactor_plan(kind="extract_rust_items")` or
  `bbox_refactor_plan(kind="extract_rust_impl_methods")` or
  `bbox_refactor_plan(kind="delete_rust_items")` or
  `bbox_refactor_plan(kind="add_rust_router_to_sum")` or
  `bbox_refactor_plan(kind="add_rust_mod_decl")` or
  `bbox_refactor_plan(kind="add_rust_use_decl")` or
  `bbox_refactor_plan(kind="copy_rust_mod_decls")` or
  `bbox_refactor_plan(kind="rewrite_rust_mod_visibility")` or
  `bbox_refactor_plan(kind="rewrite_rust_item_visibility")` or
  `bbox_refactor_plan(kind="rewrite_rust_field_visibility")` or
  `bbox_refactor_plan(kind="rust_lsp_rename")` or
  `bbox_refactor_plan(kind="rust_organize_imports")`, then
  `bbox_refactor_apply(confirm=true)`. Use `bbox_refactor_run(confirm=true)`
  when several primitive plans must succeed or rollback together. The
  rust-analyzer-backed plan kinds (`rust_lsp_rename`, `rust_organize_imports`)
  go through the warm `LspSessionManager`: first call per project pays the
  cold-start cost, subsequent calls reuse the same `(project_root, Rust)`
  child until idle eviction (`BLACKBOX_LSP_IDLE_SECS`). Tunables:
  `RUST_ANALYZER_BIN` (binary override) and
  `BLACKBOX_RUST_ANALYZER_INIT_TIMEOUT_SECS` (default 60).
- TypeScript / JavaScript: inspect-only today. Use `sm-refactor-typescript`.
- C#: inspect-only today. Use `sm-refactor-csharp`.
- Python: inspect-only today. Use `sm-refactor-python`.
- Java: supports extract methods/classes, composite extract-class handoffs, field/constructor/delegate wiring, caller delegation, extract interface, add implements, visibility rewriting, type-use migration, and JDTLS/fallback import organize. Use `sm-refactor-java`.
- Go: inspect-only today. Use `sm-refactor-go`.
- C / C++: inspect-only today. Use `sm-refactor-c-cpp`.
- Other supported tree-sitter languages: inspect-only today unless a newer
  language memory says otherwise.

## Routing

- Rust files (`.rs`) -> pull `sm-refactor-rust`.
- TypeScript or JavaScript files (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs`)
  -> pull `sm-refactor-typescript`.
- C# files (`.cs`) -> pull `sm-refactor-csharp`.
- Python files (`.py`) -> pull `sm-refactor-python`.
- Java files (`.java`) -> pull `sm-refactor-java`.
- Go files (`.go`) -> pull `sm-refactor-go`.
- C/C++ files (`.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx`) ->
  pull `sm-refactor-c-cpp`.
- Mixed-language projects -> pull every relevant language memory, then plan one
  language backend at a time.

## Common Protocol

1. Find symbols and line ranges structurally before reaching for `rg`:

```text
bbox_code_symbols(
  project_dir="/absolute/project/root",
  query="methodOrTypeName",
  languages=["java"],
  item_kinds=["method_declaration"],
  limit=20
)
```

Use this for the common "where is this method?" and "what line range is this
symbol on?" cases. The response returns `file`, `kind`, `name`, `byte_range`,
`line_range`, truncation metadata, and handoff calls. The default scan budget is
large enough for normal monorepos; if `truncated=true`, narrow with
`path_contains`, `languages`, `item_kinds`, or pass a more deliberate
`file_limit`. `rg` is still fine for unsupported file types, literal
prose/config search, or broad text audits, but it should not be the first tool
for supported source-code symbol line numbers.

2. Explore local syntax when the target is unclear:

```text
bbox_code_node_describe(
  file="path/to/file",
  project_dir="/absolute/project/root",
  line=42,
  column=12,
  include_text=true,
  include_siblings=true
)
```

Use `bbox_code_node_describe` to learn the local grammar: node kind, named
fields, parent chain, siblings, parse health, and the nearest refactor-like
ancestor. Then use `bbox_code_query` for broader single-file pattern searches:

```text
bbox_code_query(
  file="path/to/file",
  project_dir="/absolute/project/root",
  query="(function_item name: (identifier) @name)",
  limit=50,
  include_text=true
)
```

Code-nav output is `semantic_status="syntax_only"`. It may locate syntax that
looks relevant, but it does not prove binding, import paths, macro expansion, or
type correctness. Follow the response's `handoff.refactor_status` suggestion
when you need a refactorable item name/kind; follow
`handoff.project_refs` when you need current `project_file` entity refs.

3. Inspect refactorable items before planning:

```text
bbox_refactor_status(
  file="path/to/file",
  project_dir="/absolute/project/root",
  item_kinds=["impl_method"],
  limit=50,
  include_attributes=false
)
```

For Rust, status includes top-level items plus `impl_method` entries so agents
can copy exact method names before planning an impl extraction. Omit filters for
small files only; status defaults to at most 200 returned items and reports
`total_items`, `matching_items`, `returned_items`, and `truncated`.

4. Only call `bbox_refactor_plan` for a generic plan kind listed here or for a
   language-scoped plan kind that the language memory says is writable.

5. Only call `bbox_refactor_apply` after reviewing the JSON plan. Apply requires
   `confirm=true`, registered-project path scope, clean git files by default
   unless `allow_dirty_worktree=true`, hash checks, non-overlapping edits, parse
   validation, and atomic writes. For disposable practice worktrees or isolated
   smoke tests, `allow_unregistered_paths=true` bypasses the registered-project
   requirement without disabling hash, syntax, or dirty-file checks.
   Missing target files are not inherently dirty: supported extraction plans
   that create a target should model that as an empty-original `FileEdit`.
   Do not pre-create placeholder files or pass `allow_dirty_worktree=true` just
   to let an extraction produce a new type.

   **Plan-file slot policy (RX-F1b).** `output_path` (planner write) and
   `plan_path` (applier read) both resolve under
   `$BLACKBOX_STATE_DIR/refactor/plans/`. Pass a plain relative filename such as
   `"my-plan.json"` — not an absolute path. Absolute paths and filenames that
   escape the slot via `../` traversal are rejected with
   `error.bad_input(code=plan_path_outside_slot)`. The `plan_path` value in the
   returned `RefactorPlanSummary` is the absolute on-disk path; pass only the
   filename portion (e.g. `Path::file_name`) when calling apply.

6. Run the language toolchain after apply. Tree-sitter proves syntax shape, not
   semantic binding. For compound phases, add command validation steps directly
   to `bbox_refactor_run`:

```text
{"op":"command","command":"make","args":["test"],"required":true}
{"op":"command","command":"make","args":["format"],"touches":["src/example.ext"],"required":true}
```

   The runner executes commands in `project_dir` by default. Command steps are
   validation-only unless `touches` declares paths they may mutate. Declared
   touches are snapshotted before the command and are rolled back with prior
   plan writes on required command failure.

## Refactor-atom personas (RA-B1 / RA-B2)

The atomic refactor agent layer (`design/refactor-agents.md`) ships JSON
manifests installed via `bbox_artifact_install(kind="agent", …)`. Every
refactor atom binds to a **narrow persona brofile** whose allow list is
restricted to the refactor + grounding tool surface. The narrow-allow
constraint is load-bearing: `MergedFilters::merge` is additive with
deny-wins, so an atom's `filter_overlay` can only ADD denies on top of
the brofile — it cannot narrow a permissive allow list. A refactor
atom layered on top of a general persona has no mechanical surface
restriction.

Two personas ship as reference artifacts:

- **`rust-refactor-persona`** at `examples/brofiles/rust-refactor-persona.json`.
  Allows only `bbox_code_*`, `bbox_refactor_*` (status / project_refs /
  plan / apply / run), `bbox_note`, `bbox_thread`, `bbox_pin`,
  `bbox_inspect_entity`, `bbox_hybrid_search`, `Read`, `Grep`, `Glob`.
  Disallows `Bash`, `Write`, `Edit`, `bbox_learn` / `bbox_remember` /
  `bbox_decide` / `bbox_forget` / `bbox_render`, and `bro_*`. Cargo
  validation runs through `bbox_refactor_run` command steps, never via
  `Bash`. The brofile spec is verified by
  `rust_refactor_persona_matches_design_spec` in
  `src/orchestration/brofile.rs`; allow/disallow drift fails the test.

- **`java-refactor-persona`** at `examples/brofiles/java-refactor-persona.json`.
  Same allow/disallow shape as the Rust persona — the refactor + grounding
  tool surface is language-agnostic at the MCP layer; only the lens prose
  differs. Java lens calls out `mvn` / `gradle` validation and the
  annotation-processor-invisibility caveat (Lombok `@Slf4j` / `@Data`
  generate members invisible to dependency analysis). Cross-language
  symmetry verified by `rust_and_java_refactor_personas_share_tool_surface`
  in `src/orchestration/brofile.rs`.

Refactor-atom manifests under `examples/agents/refactor/*.json` MUST
bind to one of these personas via `brofile_ref`. Authoring an atom that
binds to a different brofile (e.g., `code-reviewer-persona`) means the
atom's tool surface is not actually narrowed; the manifest lint pass
(RA-S1) rejects such manifests on install.

## Refactor atom catalog

Shipped refactor atoms (RA-A* / RA-X*) — discover via
`bro_agent_search(<intent phrase>)`, install via
`bbox_artifact_install(kind="agent", source="examples/agents/refactor/<name>.json")`,
dispatch via `bro_agent_dispatch(agent="<name>", args={...})`.

| Atom | Status | Cost | Plan kind(s) | Purpose |
|---|---|---|---|---|
| `rust-impl-partition-graph` | shipped (v1, RA-A1) | cheap | `rust_impl_partition_analysis` (RX-G1) | Produce method/field/call graph for a Rust impl block; analysis-only |
| `rust-public-api-guard` | shipped (v1, RA-A2) | normal | `rust_public_api_guard` (RX-G2) | Report public-API delta of a proposed refactor as advisory severity; preflight for mutating atoms |
| `rust-test-island-extract` | shipped (v1, RA-A3) | normal | `extract_rust_items`, `add_rust_mod_decl`, `rust_compile_fix_round` | Peel inline #[cfg(test)] mod tests blocks into sibling src/tests/*.rs files |
| `rust-state-extract` | shipped (v1, RA-A4) | normal | `extract_rust_items`, `move_rust_struct_fields`, `add_rust_delegate_field`, `update_rust_callers`, `rust_compile_fix_round` | Pull a self.<field> cluster into a separate struct + delegate; operator-authority `acknowledge_repr` for #[repr(C)] / #[repr(packed)] |
| `rust-trait-from-impl` | shipped (v1, RA-A5) | normal | `extract_rust_trait`, `migrate_rust_type_usages`, `rust_compile_fix_round` | Lift method subset into trait + impl Trait for Struct; HARD REFUSES migrate_call_sites=true when dyn_compatible=false |
| `rust-error-migrate` | shipped (v1, RA-A6) | normal | `rewrite_rust_error_type` (RX-E1), `rust_public_api_guard` (preflight, RX-G2), `rust_compile_fix_round` (RX-C1) | Rewrite a module's error type; `rust_public_api_guard` runs as PREFLIGHT (outside the mutating run); operator-authority `acknowledge_public_api_change` |
| `rust-split-god-impl` | shipped (v1, RA-A7, headline) | expensive | `extract_rust_impl_methods`, `rust_ra_classify_callbacks` (RX-R2, REQUIRED), `add_rust_router_to_sum`, `add_rust_mod_decl`, `rewrite_rust_item_visibility`, `rust_organize_imports`, `rust_compile_fix_round` | Carve multi-domain impl block into per-domain modules; mandatory RA-backed cross-partition call classification; fail-closed on lsp_unavailable (RX-V3) |
| `java-extract-cohesive-class` | shipped (v1, RA-X1, Java headline) | normal | `extract_java_class` (composite) | Extract cohesive cluster (methods + field moves + delegate + caller delegation + accessor rewrites + cross-package widening) in one composite plan; hard refusals on mutable_capture_with_write / nested_class_in_item_names / method_overload_ambiguous |
| `java-promote-inner-class` | shipped (v1, RA-X2) | normal | `promote_java_inner_class` | Promote a non-static inner class with outer captures into a top-level class with final ctor params; hard refusals on static_inner / writes_outer_field / calls_outer_method / multiple_ctors / this_chain_ctor / referenced_as_type |
| `java-extract-interface` | shipped (v2, RA-X3, supersedes v1) | normal | `extract_java_interface` + `java_public_api_guard` (preflight) + optional `migrate_java_type_usages` | Extract interface from class (signatures + implements + visibility widening) with optional caller migration; v2 runs the public-API guard as a structured preflight (closes JAVA_GAP.md Gap 1); operator-authority `acknowledge_public_api_change` gates apply on `advisory_severity=breaking` |
| `java-lombokify` | shipped (v1, RA-X4) | expensive | `lombokify_java_class` (single-file or bulk-dir) + optional `java_lsp_organize_imports` | Convert hand-rolled POJO boilerplate (getters/setters/equals/hashCode/toString/canonical ctors/SLF4J) into Lombok annotations; operator-authority `boolean_getter_strategy` (skip / bridge / rename); cost_class=expensive to match bulk-dir worst case |
| `java-public-api-guard` | shipped (v1, RA-X5) | normal | `java_public_api_guard` | Report public-API delta of a proposed Java refactor as advisory severity; preflight for `java-extract-interface` and standalone audit; closes JAVA_GAP.md Gap 1 |
| `java-class-dependency-graph` | shipped (v1, RA-X6) | cheap | `java_class_dependency_analysis` | Class-shaped inventory — methods + fields + inner types + class-level annotations — for operator review before partition decisions; analysis-only preflight to `java-extract-cohesive-class`; closes JAVA_GAP.md Gap 2 |
| `java-find-usages` | shipped (v1) | cheap | `find_java_usages` | Project-wide reference walk for one or more simple Java names with optional `declaring_class` filter; production_sites/test_sites tally; optional `output_path` + `summary_only` for large reports; analysis-only sibling to `java-public-api-guard` |

Per-atom manifests live at `examples/agents/refactor/<atom>.json`. The
shared prompt template and base outputs schema (RA-T1) under the same
directory are reference files — manifest installers inline the
filled-in form. `sm_refactor_catalog_lists_every_shipped_atom` (RA-D1)
asserts every `examples/agents/refactor/<name>.json` has a row in
this table; new atoms that land without a catalog entry fail the
build. The mechanical alternative to a manually maintained table —
auto-regeneration from the manifest set — is a `tools/refactor-atom-
catalog-gen` follow-up.

## Refactor-atom eval coverage (RA-E1)

Per-atom eval artifacts live under `eval/agents/refactor/`:

- `discovery-queries.json` — per-atom queries matching `when_to_use`,
  plus anti-pattern queries with `expect_matched_anti_pattern: true`.
  Runs in **keyword-only mode** in CI (`include_vectors=false`) to
  stay deterministic. Vector-ready mode is gated on every shipped
  atom showing `embedding_pending=false` and tolerates ranking
  fluctuation.
- `dispatch-scenarios.json` — per-atom install + dispatch round trip
  with a fixture input and an `expect_response_fields` shape
  assertion. The fixture paths use `${FIXTURE_RUST_PROJECT}` for the
  Rust project root; future eval-runner integration resolves them.
- `behavior-smoke.json` — per-atom `expected_plan_sequence` listing
  the bbox_refactor_* tool calls the atom's prompt template
  encodes, plus an optional `block_reachability` fixture and the
  expected block_reason substring. NOT exhaustive semantic
  testing (that's RX-per-plan-kind territory).

`refactor_atom_eval_suites_cover_every_shipped_atom` (RA-E1) asserts
every atom under `examples/agents/refactor/<name>.json` appears in all
three eval suites; eval drift fails the build.

The **recording AgentDispatchAdapter** that interprets behavior-smoke
entries against the prompt template (registering a fake adapter per
the impl doc's Codex round-2/3 design, parsing the template's
bbox_refactor_* markers in order and asserting the simulated tool-call
sequence matches `expected_plan_sequence`) is tracked as a follow-up.
v1 ships the deterministic artifact alignment; live LLM dispatch is a
secondary integration check (marked slow/live) that requires the
adapter implementation.

## Refactor-atom supersession (RA-Z1)

Atom version bumps use the standard artifact-supersede mechanism:

```
bbox_artifact_supersede(kind="agent", name="<atom>", superseded_by="<atom>")
```

Concretely, installing a new version with `"supersedes": "<atom>"` in
the artifact body marks the prior version superseded automatically
(`install_value_locked_scoped` handles the supersedes chain).

Behavior pinned by `supersession_hides_old_versions_from_search_and_list`:

- `bro_agent_search` excludes superseded versions. Default-search
  surfaces only the active version.
- `bro_agent_list` without `include_superseded=true` excludes
  superseded versions; both surface when `include_superseded=true`.
- `bro_agent_get(name="<atom>@v<N>")` and `bro_agent_describe(...)`
  resolve superseded versions (read paths permit `include_superseded
  =true`).
- `bro_agent_dispatch(agent="<atom>@v<N>")` REJECTS superseded
  versions with `agent '...' is not active (superseded or
  deactivated)` (`src/tools/agents.rs:522`). v1 does NOT add an
  `allow_superseded` flag to dispatch — the active version is
  canonical. Operators who need to dispatch an older version must
  explicitly un-supersede it through the existing
  `bbox_artifact_supersede` mechanics, or pin to a still-active
  version.

Every new atom version bump triggers an embedding refresh in the
`agent_manifest` bucket (existing agent-system behavior; called out
here so operators understand the cost per version bump).

Removal/cleanup of ancient superseded versions is a separate decision;
v1 keeps history indefinitely.

## Refactor-atom distillation path (RA-V2)

The v1 catalog is `provenance: hand_authored` end-to-end. The schema
already carries `AgentProvenance::Distilled` (`src/orchestration/agents/
types.rs:170`) — distilled atoms enter the catalog by the same path
as hand-authored atoms (`bbox_artifact_install(kind="agent",
source=…)`) but declare:

- `provenance.kind = "distilled"`
- `provenance.distilled_by` — agent or pipeline name that produced
  the manifest
- `provenance.evidence_session_ids` — session refs the distiller
  mined for the pattern
- `provenance.created_from_threads` — thread refs the distiller
  walked

The install path materializes agentic-corpus edges from distilled
manifests back to source sessions/threads automatically
(`src/server/routes.rs::persist_agent_provenance_edges`).

The distiller itself is **out of scope for this skeleton**. A
badgey-flavor pipeline that mines the corpus for recurring refactor
task shapes and proposes new atoms is acknowledged in
`design/refactor-agents.md` "Provenance — distillation path" and
tracked separately from RA-* phases. When the distiller lands, the
RA-S1 refactor-atom lint applies to distilled manifests
unchanged — the `_contract: "refactor-atom/v1"` marker and the
recognized-personas list bind identically regardless of provenance.

## Refactor-atom composition (RA-V1)

v1 atom composition is hand-wired through workflows. The
`composition.chainable_after`, `parallel_safe`, and `fan_out_aggregator`
fields on each manifest are signals to workflow authors and to the
agent-search ranker — the manifest fields do not autoload at runtime
per `design/agent-system-impl.md` §608. There is no `bro_agent_compose`
consumer in v1; atom-to-atom dispatch is a v2 path.

Three canonical composition shapes ship as reference workflow JSONs
under `examples/agents/refactor/workflows/`:

- **`state-extract-then-split.json`** — `rust-state-extract` →
  `rust-split-god-impl`. State extraction lands first so the
  partitioned impl references a clean state struct via the
  delegate field.
- **`error-migrate-with-guard.json`** — `rust-public-api-guard` →
  `rust-error-migrate`. The workflow-level guard call gives the
  operator a separate audit trail; the migrate atom additionally
  runs its OWN preflight internally and blocks on
  acknowledge_public_api_change unless operator-explicit.
- **`partition-graph-then-split.json`** — `rust-impl-partition-graph` →
  operator review → `rust-split-god-impl`. The graph atom produces
  structural facts; the operator-supplied partition variable drives
  the splitter.

Reference workflows install through `bbox_artifact_install(kind=
"workflow", source="examples/agents/refactor/workflows/<name>.json")`
and dispatch via `bro_orchestrate_run(workflow="refactor-<name>",
vars={...})`. The test
`refactor_atom_reference_workflows_parse_and_compile` asserts each
one parses as a `Workflow` and compiles cleanly; drift fails the
test.

Fan-out across languages is supported through the workflow engine's
existing `Fork` / `Wait` primitives — `parallel_safe: false` on
individual atoms doesn't prevent fan-out, because parallel dispatches
run against DIFFERENT files / projects, not against each other.
Future v2 composition (`bro_agent_compose`) is acknowledged in
`design/agent-system-impl.md` §608 but is not required by v1.

## Refactor-atom install lint (RA-S1)

A manifest is treated as a refactor atom — and subject to the refactor-atom
lint — when one of:

- Top-level `"_contract": "refactor-atom/v1"` field is present (authoring
  convention; refactor atom templates include this).
- Artifact source path contains `examples/agents/refactor/`.

There is no opt-out flag in v1: an operator who wants different semantics
either drops the contract marker or hosts the manifest outside the refactor
path. `schema/agent.schema.json` has `additionalProperties: false` at the
manifest level, so adding a top-level escape-hatch field would itself be
rejected by the generic schema.

**Hard rejects** (install fails with
`error.bad_input(code=refactor_atom_lint_failed)`):

- `brofile_ref` is not one of `rust-refactor-persona` /
  `java-refactor-persona`. Refactor atoms layered on a permissive persona
  have no mechanical tool-surface restriction — `filter_overlay` can only
  ADD denies.
- `inputs.schema` declares an `acknowledge_*` field with a `default` value
  (any default). Operator-authority opt-outs must be operator-explicit per
  RX-V1.

**Warnings** (install succeeds, surfaced via `install_warnings`):

- `filter_overlay.allow` non-empty — refactor atoms narrow via additional
  denies only.
- `outputs.schema` drops one or more RA-T1 base fields (status, plan_path,
  files_touched, fixme_count, deep_analysis_summary, cargo_result,
  block_reason, done_note_id).
- `inputs.prompt_template` is missing one or more protocol markers
  (`bbox_refactor_plan`, `bbox_refactor_run`, `bbox_note(kind=`).
  Analysis-only atoms legitimately skip `bbox_refactor_run`; the warning
  is informational in that case.

## Shared atom contract (RA-T1)

Every refactor atom embeds the same five-step protocol
(ground → plan with `deep_analysis=true` → decide → apply-or-block →
done-note) and the same base outputs.schema (status / plan_path /
files_touched / fixme_count / deep_analysis_summary / cargo_result /
block_reason / done_note_id). Reference files:

- `examples/agents/refactor/_template.prompt.md` — the prompt template
  with `{{...}}` placeholders. Atom manifests inline the filled-in form
  under `inputs.prompt_template`. The artifact installer does not yet
  support shared-template includes; the `tools/refactor-atom-fill`
  helper (follow-up) keeps manifests in sync mechanically.
- `examples/agents/refactor/_base.outputs.schema.json` — the base
  outputs.schema every atom unions with atom-specific fields. Advisory
  in v1: dispatch does not validate the agent's emission against the
  schema; the RA-S1 lint warns when a manifest's `outputs.schema`
  drops one or more of the base fields.

`fixme_count` is split into `plan_only` (FIXMEs the plan emitted with
no associated edit) and `warning` (FIXMEs emitted alongside an applied
edit, flagging operator follow-up) per the two-prefix FIXME grammar in
`sm-refactor-rust`.

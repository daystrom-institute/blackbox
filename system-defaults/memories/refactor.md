+++
title = "Refactor mechanization catalog — language routing and support matrix"
tags = ["refactor", "refactoring", "mechanization", "restructure", "language", "catalog", "support-matrix", "tree-sitter", "bbox_refactor_status", "bbox_refactor_plan", "bbox_refactor_apply", "rust", "typescript", "javascript", "csharp", "python", "java", "go", "c", "cpp", "sm-refactor-rust", "sm-refactor-typescript", "sm-refactor-csharp", "sm-refactor-python", "sm-refactor-java", "sm-refactor-java-extract-class", "sm-refactor-java-lombokify", "sm-refactor-go", "sm-refactor-c-cpp"]
order = 7
template = false
+++
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
one file without re-parsing yourself; curated tree-sitter queries cover Rust,
Java, Python, TypeScript, JavaScript, Go — other languages return a typed
`unsupported_language_for_code_refs` error for non-`identifiers` kinds.
`kind="identifiers"` falls back to a generic walker that emits records for
nodes literally named `identifier`; grammars that use different
identifier-like kinds (Erlang's `atom`/`variable`, e.g.) will return zero
records — use `bbox_code_query` with a grammar-native S-expression when that
happens. Every response carries
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
- `invalid_code_refs_kind` — `bbox_code_refs(kind=...)` was something
  other than `"calls"` / `"imports"` / `"fields"` / `"identifiers"` /
  `"all"`.
- `unsupported_language_for_code_refs` — language has no curated
  reference query and the requested `kind` is not `"identifiers"`. Use
  `bbox_code_query` instead, or switch to `kind="identifiers"`
  (shape-only fallback; may return zero on grammars that don't use
  `identifier` nodes).

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

## Refactor-atom personas

The refactor atom layer ships JSON manifests installed via
`bbox_artifact_install(kind="atom", …)`. Every refactor atom uses
`subcontract: "refactor/v1"` and binds to a **narrow persona brofile** through
`manifest.implementation.brofile_ref`. The persona's allow list is the
mechanical boundary: profile-backed atom execution dispatches through that
brofile, so the atom inherits the refactor + grounding tool surface rather
than a general-purpose operator surface.

Two personas ship as reference artifacts:

- **`rust-refactor-persona`** at `system-defaults/brofiles/refactor/rust-refactor-persona.json`.
  Allows only `bbox_code_*` (symbols / node_describe / query / refs),
  `bbox_refactor_*` (status / project_refs / plan / apply / run),
  `bbox_note`, `bbox_thread`, `bbox_pin`, `bbox_inspect_entity`,
  `bbox_hybrid_search`, `Read`, `Grep`, `Glob`.
  Disallows `Bash`, `Write`, `Edit`, `bbox_learn` / `bbox_remember` /
  `bbox_decide` / `bbox_forget` / `bbox_render`, and `bro_*`. Cargo
  validation runs through `bbox_refactor_run` command steps, never via
  `Bash`. The brofile spec is verified by
  `rust_refactor_persona_matches_design_spec` in
  `src/orchestration/brofile.rs`; allow/disallow drift fails the test.

- **`java-refactor-persona`** at `system-defaults/brofiles/refactor/java-refactor-persona.json`.
  Same allow/disallow shape as the Rust persona — the refactor + grounding
  tool surface is language-agnostic at the MCP layer; only the lens prose
  differs. Java lens calls out `mvn` / `gradle` validation and the
  annotation-processor-invisibility caveat (Lombok `@Slf4j` / `@Data`
  generate members invisible to dependency analysis). Cross-language
  symmetry verified by `rust_and_java_refactor_personas_share_tool_surface`
  in `src/orchestration/brofile.rs`.

Refactor-atom manifests under `system-defaults/atoms/refactor/*.json` MUST
bind to one of these personas via a typed ref such as
`brofile:rust-refactor-persona@v1`. Authoring an atom that binds to a
different brofile (e.g., `code-reviewer-persona`) means the atom's tool
surface is not actually narrowed; the `refactor/v1` atom subcontract rejects
such manifests on install.

## Refactor atom discovery

System memory is not the atom catalog. Do not mirror shipped atom names,
versions, status, costs, eval fixtures, or release lineage here. The active
catalog lives in installable manifests and the artifact tools:

```text
atom_search(query="<intent phrase>")
atom_describe(atom="atom:<name>@latest")
bbox_artifact_list(kind="atom", name="<optional name>")
```

Mention atoms in system memory only as contextual signposts: when a documented
tool sequence has a reusable atom boundary, say "for this pattern, consider the
matching refactor atom via `atom_search(...)`" and keep the primitive sequence
as the canonical fallback. The manifest is the source of truth for an atom's
version, cost class, input schema, prompt, implementation brofile, and
operator-authority flags. Pull `sm-atoms` only when you need the deeper atom
contract: backend kinds, invocation handles, effect limits, child composition,
workflow bindings, or manifest authoring rules.

Workflow composition is also a catalog concern. Use workflow artifacts when a
refactor needs multiple atom boundaries with gates or operator review between
them; do not paste a workflow inventory into resident memory.

## Refactor-atom install validation

A manifest is treated as a refactor atom when it declares
`subcontract: "refactor/v1"`. The atom install validator hard-rejects:

- `manifest.implementation.kind` other than `profile`.
- `manifest.implementation.brofile_ref` not in
  `brofile:rust-refactor-persona@vN` or `brofile:java-refactor-persona@vN`.
- `inputs.schema` declaring any `acknowledge_*` field with a `default` value.
  Operator-authority opt-outs must be operator-explicit.

## Shared atom contract

Every refactor atom embeds the same five-step protocol
(ground → plan with `deep_analysis=true` → decide → apply-or-block →
done-note) and the same base outputs.schema (status / plan_path /
files_touched / fixme_count / deep_analysis_summary / cargo_result /
block_reason / done_note_id). Reference files:

- `system-defaults/atoms/refactor/_template.prompt.md` — the prompt template
  with `{{...}}` placeholders. Atom manifests inline the filled-in form
  under `inputs.prompt_template`.
- `system-defaults/atoms/refactor/_base.outputs.schema.json` — the base
  outputs.schema every atom unions with atom-specific fields.

`fixme_count` is split into `plan_only` (FIXMEs the plan emitted with
no associated edit) and `warning` (FIXMEs emitted alongside an applied
edit, flagging operator follow-up) per the two-prefix FIXME grammar in
`sm-refactor-rust`.

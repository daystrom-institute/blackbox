---
tags:
  - refactor-tools
  - elixir
---
+++
title = "Elixir refactor mechanization — atom-tag dispatch decomposition + GenServer concern extraction"
tags = ["refactor", "refactoring", "mechanization", "restructure", "elixir", "ex", "exs", "tree-sitter", "bbox_refactor_status", "bbox_refactor_plan", "bbox_refactor_apply", "split_elixir_clauses_by_tag", "extract_elixir_module", "extract_genserver_callback_group", "mix", "elixir-ls", "beam", "genserver", "behaviour"]
order = 12
template = false
+++
# Elixir Refactor Mechanization Runbook

Use this memory before moving, extracting, renaming, or splitting Elixir code
with blackbox refactor tools. The design surface is
`design/refactor-tools/elixir/refactor-elixir-expansion.md`.

## Atom signposts

For recurring Elixir refactor patterns, check `atom_search(query="<intent>")`
before re-deriving the whole tool sequence. Use atoms as contextual shortcuts:

- `elixir-shatter-dispatch-table` ★ keystone — decompose a multi-clause
  atom-tag-dispatch function (def foo(:tag1, ...)) into per-tag submodules
  with a router (`op_runtime.ex` shape).
- `elixir-split-genserver` — carve a GenServer into per-concern child
  GenServers (`admin_endpoint.ex` shape; both single_dispatch_fn and
  per_message_handle_call dispatch patterns).
- `elixir-facade-wire` — regenerate `defdelegate` blocks on a facade
  module (`substrate.ex`-style facades).
- `elixir-extract-behaviour` — lift function set → `@behaviour` + `@callback`.
- `elixir-organize-aliases` — sort/dedupe/collapse alias/import/require/use.
- `elixir-public-api-guard` — pre-flight before refactors touching public
  surface. Advisory only.
- `elixir-module-dependency-graph` — Tier-1 static call graph for a dir.
- `elixir-genserver-state-audit` — per-callback state-field reads/writes;
  precondition for `elixir-split-genserver`.

## Plan kind catalog (v1)

`bbox_refactor_plan(kind="...")` dispatches the following Elixir kinds:

| Plan kind | Semantic tier | Notes |
|-----------|---------------|-------|
| `split_elixir_clauses_by_tag` ★ | indexed_hints | Keystone. Single-fn, primary-discriminator-only in v1. |
| `extract_elixir_module` | syntax_only | Move named def/defp/defmacro to new module. |
| `extract_genserver_callback_group` | indexed_hints | Both dispatch shapes; supports `support_callbacks` for async groups. |
| `add_elixir_facade_delegations` | syntax_only | Mirror backing module publics. |
| `extract_elixir_behaviour` | indexed_hints | Lift def set to @behaviour. |
| `elixir_organize_aliases` | syntax_only | Sort/dedupe/collapse directives. |
| `elixir_module_dependency_analysis` | indexed_hints | Analysis-only graph. |
| `elixir_public_api_guard` | indexed_hints | Analysis-only delta report. |
| `elixir_genserver_state_audit` | indexed_hints | Analysis-only state schema. |
| `inline_elixir_module` | syntax_only | Reverse of extract_elixir_module. |
| `elixir_pipe_chain_extract` | indexed_hints | Subsequence → defp. |
| `elixir_with_clause_extract` | indexed_hints | Prefix → fn returning {:ok, _}. |
| `rename_elixir_symbol` | lsp_verified (refuses in v1) | Probe-or-refuse only. |
| `elixir_codegen_audit` | syntax_only | Analysis-only quote-block report. |
| `elixir_test_fixture_extract` | syntax_only | Pull duplicated setup blocks. |
| `elixir_move_module_across_apps` | indexed_hints | Umbrella module move + mix.exs advisory. |
| `elixir_compile_fix_round` | syntax_only | Mix compile diagnostic → edit proposals. |
| `elixir_credo_fix_round` | syntax_only | Credo JSON lint → edit proposals. |
| `elixir_dialyzer_attribution` | indexed_hints | Dialyzer warning attribution; report-only. |

All 19 v1 plan kinds ship. The fix-round kinds operate in two modes:
1. **Subprocess fallback (v1 default):** parse mix compile / credo / dialyzer
   stdout-stderr via the stable text formats (Credo + Dialyzer JSON/short
   formats are version-stable; mix compile stderr is parsed by the
   helper module's stable regex).
2. **Escript-driven (v2 path):** when `priv/elixir_ast_helper/` is built,
   `compile_diagnostics` uses `Code.with_diagnostics/2` for structured
   capture without depending on stderr formatting.

## Operator-authority acknowledgments (EX-V1)

Atoms NEVER default these; operator passes them explicitly:

- `acknowledge_use_at_scope` — source has `use Foo` at module scope (always
  required for GenServer / Phoenix / Ecto modules).
- `acknowledge_quote_in_moved` — moved body contains `quote do … end`.
- `acknowledge_defmacro_move` — moved item is `defmacro`.
- `acknowledge_unpreservable_guards` — `split_elixir_clauses_by_tag` only;
  accept that the planner drops the un-preservable subset of guards.
- `acknowledge_dynamic_dispatch` — operator confirms `apply/3` /
  `Module.concat` sites in moved clauses don't dispatch to unmoved helpers.
- `acknowledge_shared_state` — `extract_genserver_callback_group` only.
- `acknowledge_anonymous_fn_callbacks` — `extract_elixir_behaviour` only.
- `acknowledge_attribute_scope`, `acknowledge_describe_context` —
  `elixir_test_fixture_extract` only.
- `acknowledge_else_arm_residue` — `elixir_with_clause_extract` only.
- `acknowledge_app_boundary_crossing` — `elixir_move_module_across_apps` only.
- `acknowledge_public_api_change` — any plan kind touching public surface.

Plan responses carry `operator_opt_outs_used` recording which flags were
consumed (named individually, not collapsed). v2: the runner verifies the
audit list matches expectations.

## Compose-run protocol

The Mix-only command allowlist (EX-V2) for atom-dispatched runs:

- `mix compile` (any args; read-only)
- `mix test` (any args)
- `mix credo` (any args; read-only)
- `mix dialyzer` (any args)
- `mix format --check-formatted` (read-only; no `touches` requirement)
- `mix format` without `--check-formatted` (mutating; `touches` required)
- `mix xref` (any args; read-only)

Canonical refactor-run sequence:

```jsonc
bbox_refactor_run(dispatch_origin="agent", confirm=true, steps=[
  <plan step>,
  {"op": "command", "command": "mix",
   "args": ["compile", "--warnings-as-errors", "--return-errors"],
   "capture": "mix_diag", "on_failure": "continue_for_repair"},
  {"op": "plan", "kind": "elixir_compile_fix_round",
   "diagnostics_ref": "last"},                       // v2; deferred
  {"op": "command", "command": "mix", "args": ["compile", "--warnings-as-errors"],
   "required": true},
  {"op": "command", "command": "mix", "args": ["format", "--check-formatted"],
   "required": true},
  {"op": "command", "command": "mix", "args": ["test", "--max-failures", "1"],
   "required": true}
])
```

## LSP availability

`rename_elixir_symbol` is `lsp_verified` per the capability matrix but
**v1 refuses for every symbol kind** — elixir-ls and lexical have no
working `textDocument/rename` provider as of 2026-05. The kind exists as
a structured-refusal surface so callers don't reinvent syntactic rename
(which would unsafely miss `name: __MODULE__` registrations, supervisor
child specs, and `@behaviour` references). Operators perform v1 renames
manually via editor refactor.

elixir-ls's `manipulatePipes` and `expandMacro` execute-commands ARE
available; `elixir_pipe_chain_extract` and `elixir_codegen_audit` will
delegate to them in v2 for the normalization / expansion sub-steps.

## EX-V6 round-trip preservation

Every plan kind that emits FileEdits MUST go through the writable AST
lane (`Code.string_to_quoted_with_comments!/2`) and pass an apply-time
round-trip check: parse output, compare AST structure (ignoring `:line`,
`:column`, `:end_line`, `:end_column`, `:end_of_expression`, `:closing`,
`:format`, `:merge`, `:token`; preserving `:context`, `:delimiter`,
`:do`/`:do_end`), compare comment anchors (next-following-AST-node
attachment) + bodies. Plans that legitimately delete a comment declare
it in `expected_comment_deletions`.

v1 ships the round-trip skeleton in `roundtrip_check` field of EX-G1's
plan response (passed: null = deferred to apply); v2 implementation
performs the check during apply.

## Substrate decisions (v1 vs v2)

- **AST parsing**: tree-sitter-elixir via `tree_sitter_language_pack` in v1
  for all syntactic analysis. The escript-based writable lane
  (`Code.string_to_quoted_with_comments!/2`) ships in v2; v1 plan kinds
  operate on tree-sitter trees and rely on apply-time EX-V6 round-trip
  via the helper.
- **AST helper**: daemon-managed escript with project-root pinning (per
  Open Question 1 resolution); v1 plan kinds work without it; v2 plan
  kinds (`elixir_compile_fix_round`, etc.) require it.
- **Alias resolution**: literal-text in v1
  (`B.bye()` after `alias App.B` records as `B`, not `App.B`).
  v2 resolves through alias context. Cycle detection currently misses
  intra-project cycles that bridge through aliases.

## Common acceptance smoke

After applying any structural plan, the operator (or chained atom)
should confirm:

1. `mix compile --warnings-as-errors` returns 0 (no new diagnostics).
2. `mix format --check-formatted` returns 0 (no formatter drift).
3. `mix test --max-failures 1` returns 0 (no test regressions).

The default refactor-run step sequence above wires all three.

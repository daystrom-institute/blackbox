---
title: "Elixir Refactor Expansion - plan kinds for atom-tag dispatch, GenServer concerns, and facade delegation"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - elixir
tags:
  - refactor-tools
  - elixir
  - beam
date: 2026-05-15
status: "design proposal, pure design (no implementation phasing)"
brief: "Designs an Elixir refactor surface mirroring the Rust/Java taxonomy where it transfers and adding plan kinds for the BEAM-specific patterns (multi-clause atom-tag dispatch, GenServer triplet boilerplate, defdelegate facades, quote-based codegen)."
---

# Elixir Refactor Expansion — plan kinds for atom-tag dispatch, GenServer concerns, and facade delegation

Related: `../refactor-tools.md`, `../refactor-agents.md`,
`../rust/refactor-rust-expansion.md`, `../rust/rust-refactor-gap-inventory.md`,
`../java/java-refactor-gaps.md`, `../ast-refactor-mechanization.md`,
`../refactor-compound-runs.md`,
`sm-refactor`, `sm-refactor-rust`, `sm-refactor-java`

## Problem

The Rust and Java refactor surfaces cover the OO and Rust-flavor god-shapes
well: god-impls, god-classes, god-traits, error-type churn, public-API drift,
visibility rewrites, caller migration, supertrait/interface extraction. Their
plan-kind taxonomy assumes a substrate where the cohesion unit is a class or
impl block, the dispatch unit is a method or trait method, and the dominant
analysis tier is borrow/ownership (Rust) or capture/dependency (Java).

Elixir codebases concentrate complexity in a fundamentally different shape:

1. **Multi-clause function dispatch on a leading atom tag** is the canonical
   Elixir/Erlang idiom. Adding "another case" means appending a new
   `def foo(:tagN, args)` clause. There is no class envelope to push back on
   unbounded growth. A single function name in one module ends up holding
   dozens to hundreds of clauses, each effectively a small program.
2. **GenServer / `gen_statem` triplet boilerplate.** Every public synchronous
   call materializes as three coupled definitions: a client-API wrapper
   (`def status, do: GenServer.call(server(), :status, :infinity)`), a
   `handle_call` (or a single `handle_call` routing to a `defp dispatch/1`),
   and an inner dispatch clause. The three live in one file and must move
   together; existing primitives don't model the triplet.
3. **Defdelegate facades.** The canonical umbrella shape is a top-level module
   forwarding hundreds of names to backing modules through `defdelegate`.
   These facades drift as backing modules change.
4. **Macro/codegen surfaces.** `quote do defmodule unquote(module) do ... end
   end` generates whole modules at compile time. The generated code isn't on
   disk; refactor primitives that read the source AST miss what actually
   compiles.
5. **Behaviour adoption.** Promoting an ad-hoc API to a `@behaviour` with
   `@callback` decls is the Elixir analogue of trait/interface extraction, but
   the shape differs from both (typespec-driven, opt-in `@impl true`).

The Rust and Java tooling cannot be retargeted at the Elixir AST and yield
useful refactors. Tree-sitter Elixir is incomplete; the authoritative AST is
`Code.string_to_quoted/2`. The compile gate is `mix compile`, not `cargo
check` or `mvn`. The dialyzer pass is success-typing, with no parallel in
either of the existing language surfaces.

### Grounded pain inventory

Concrete examples from a representative Elixir codebase
(`erlang-test/apps/substrate`, `erlang-test/apps/witness` — pure Elixir
despite the umbrella name):

| File | LOC | Shape |
|---|---|---|
| `apps/substrate/lib/substrate/op_runtime.ex` | 5,175 | 158 `def run(data, %Op{kind: :X})` clauses (incl. duplicate-tag pairs at `:author_edge` 570/590 and `:emit_phase_evidence` 1674/1680) |
| `apps/substrate/lib/substrate/admin_endpoint.ex` | 2,796 | ~50 client-API wrappers + single `handle_call` → `defp dispatch/1` with ~60 clauses |
| `apps/substrate/lib/substrate/bootstrap.ex` | 7,501 | GenServer with large hardcoded fixture corpus |
| `apps/substrate/lib/substrate/epistemics/schema.ex` | 4,790 | Validation predicates as multi-clause heads + literal tables |
| `apps/witness/lib/witness/verifier.ex` | 3,069 | ~30+ `defp apply_invariant(:tag, ckpt, admin)` clauses + 247 defp helpers |
| `apps/substrate/lib/substrate.ex` | 330 | Facade with 201 `defdelegate` lines |
| `apps/substrate/lib/substrate/workflow_projector.ex` | 1,103 | `quote do defmodule unquote(module) do ... end end` per-Workflow codegen |

The dominant shape across every god-module surveyed is the same: atom-tag
dispatch via multi-clause heads. No existing primitive carves it.

## Non-Goals

- Renegotiating the semantic tiers (`syntax_only` / `indexed_hints` /
  `lsp_verified`) — inherited from the Rust expansion design.
- Replacing the shared refactor substrate. The MCP surface
  (`bbox_refactor_plan`, `bbox_refactor_apply`, `bbox_refactor_run`,
  `bbox_refactor_status`, `bbox_code_symbols`, `bbox_code_node_describe`,
  `bbox_code_query`) is language-agnostic; this design adds plan-kind
  variants behind it.
- Wiring tree-sitter-elixir as the authoritative AST. `Code.string_to_quoted/2`
  is the BEAM-native parser and round-trips through `Macro.to_string/1`;
  tree-sitter is acceptable for byte-range/location lookup but not for
  authoritative edits.
- Inventing a new orchestration layer. Elixir refactor atoms reuse the
  existing agent infrastructure described in `../refactor-agents.md`.
- Auto-mechanizing macro expansion. Expanding a `defmacro` is fundamentally
  compilation, not refactor. Codegen plan kinds emit snapshots, not edits to
  generated code.
- Closing the `mix new` ↔ existing module gap (project-creation flows).
  Out of scope.

## Substrate decisions

### AST surface — two lanes

The substrate splits into a writable lane (must preserve comments and
literal trivia) and an analysis-only lane (compilable AST; can drop trivia).
Mixing them is a silent rewrite hazard.

- **Writable lane (every plan kind that emits FileEdits):**
  `Code.string_to_quoted_with_comments!/2` with
  `columns: true, token_metadata: true, literal_encoder: false,
  unescape: false, emit_warnings: false`. Comments are returned as a sidecar
  list, not embedded in the quoted form; the round-trip serializer must
  re-thread them by line/column. Round-trip via
  `Code.Formatter.to_algebra/2` (Elixir 1.13+ public API) →
  `Inspect.Algebra.format/2`, OR via the formatter's
  `Code.format_string!/2` driven by the comments sidecar. Plain
  `Code.quoted_to_algebra/2` drops comments and is not usable for
  writable edits.

  **Invariant EX-V6 (below):** every writable plan kind must pass a
  round-trip test (parse → serialize → parse → compare AST) as part of
  the apply step. The test detects formatter drift, dropped comments,
  and `token_metadata` loss. Plans that fail the round-trip refuse with
  `error.roundtrip_unstable` and the diff.

- **Analysis-only lane (deep_analysis reports, dependency graphs,
  state-field audits, no edits):** `Code.string_to_quoted!/2` with
  `columns: true, token_metadata: true`. No `:literal_encoder` (Elixir
  docs explicitly warn `literal_encoder`-wrapped AST is not valid for
  normal evaluation/compilation and the substrate does not need it for
  read-only analysis). Comments may be dropped.

- **Tree-sitter elixir:** used only for byte-range lookups, attribute
  slices, and the cheap `bbox_code_symbols` enumeration. Never
  authoritative for edits or for resolving macro-expanded names.

- **`Code.Fragment`** for cursor-anchored queries (parameter lists,
  alias/import resolution at a position). Used by LSP-backed kinds when
  the LSP isn't ground truth on a specific anchor.

### LSP

- **elixir-ls** is the v1 LSP backend (most stable, widest community use).
  **lexical** is acceptable as an alternate when configured.
- **elixir-ls rename capability (verified 2026-05):** the public feature
  list does NOT include `textDocument/rename`. The upstream tree has
  protocol struct definitions for rename but no provider implementation
  under `providers/`; `providers/execute_command.ex` exposes
  `expandMacro` and `manipulatePipes` handlers but no rename handler.
  Treat elixir-ls rename as unavailable in v1; see EX-G5 for the
  capability matrix and how `rename_elixir_symbol` degrades.
- LSP-backed plan kinds **fail closed** on LSP unavailability OR on
  unsupported symbol-kind capability per the matrix. No silent
  syntactic-rename downgrade. Invariant **EX-V3** below.
- elixir-ls's `manipulatePipes` and `expandMacro` execute-commands ARE
  available and back EX-G17 (pipe-chain refactor) and EX-G10
  (`elixir_codegen_audit`) respectively.

### Compile gate

- **`mix compile --warnings-as-errors --return-errors --force`** is the
  `cargo check` analogue. **The daemon never edits the operator's
  `mix.exs` to extract diagnostics.** Diagnostics are captured by one of
  two non-invasive paths, in preference order:
  1. **In-process via the AST helper:** the long-running helper
     (Substrate decision below) runs the compile through
     `Code.with_diagnostics/2` (Elixir 1.15+) which captures
     diagnostics as `%Code.Diagnostic{}` structs without writing to
     stdout. This is the default path.
  2. **Subprocess fallback:** when the helper is unavailable, the
     daemon parses `mix compile`'s human-readable stderr (the format
     is stable enough across 1.14–1.17 to extract file/line/message
     tuples reliably; the parser pins to a version range and refuses
     on unknown output shape with `error.diagnostic_format_mismatch`).
  Neither path installs a custom Mix formatter or otherwise mutates
  the project's build configuration.
- **`mix credo --format=json --strict`** is the clippy analogue
  (`elixir_credo_fix_round`). Credo's JSON output IS stable across
  versions (Credo owns the format) so no mix.exs side effect needed.
- **`mix dialyzer --format=short`** drives `elixir_dialyzer_attribution`.
  Dialyzer requires a built PLT; the runner snapshots `_build/` along
  with source for rollback parity, since dialyzer warnings change as
  the PLT warms.
- **`mix format --check-formatted`** post-flight on every applied plan
  that touches `.ex`/`.exs` files. This is a **read-only verification
  step** — it returns non-zero if formatting drifts, but mutates
  nothing. EX-V2's `touches` requirement applies only to mutating
  `mix format`; `--check-formatted` runs without `touches` because the
  runner has no rollback obligation for a read-only command.

### Persona brofile prerequisite

Per the refactor-agents `MergedFilters::merge` rule (overlay can only add
denies), atoms need a narrow brofile. Add **`elixir-refactor-persona`**
mirroring `rust-refactor-persona`:

```jsonc
{
  "name": "elixir-refactor-persona",
  "provider": "claude",
  "model": "claude-sonnet-4-6",
  "effort": "medium",
  "filters": {
    "allow": [
      "mcp__blackbox__bbox_code_symbols",
      "mcp__blackbox__bbox_code_node_describe",
      "mcp__blackbox__bbox_code_query",
      "mcp__blackbox__bbox_refactor_status",
      "mcp__blackbox__bbox_refactor_project_refs",
      "mcp__blackbox__bbox_refactor_plan",
      "mcp__blackbox__bbox_refactor_apply",
      "mcp__blackbox__bbox_refactor_run",
      "mcp__blackbox__bbox_note",
      "mcp__blackbox__bbox_thread",
      "mcp__blackbox__bbox_pin",
      "mcp__blackbox__bbox_inspect_entity",
      "mcp__blackbox__bbox_hybrid_search",
      "Read",
      "Grep",
      "Glob"
    ],
    "disallow": [
      "mcp__blackbox__bbox_forget",
      "mcp__blackbox__bbox_decide",
      "mcp__blackbox__bbox_learn",
      "mcp__blackbox__bbox_remember",
      "mcp__blackbox__bbox_render",
      "mcp__blackbox__bro_*",
      "Bash",
      "Write",
      "Edit"
    ]
  },
  "lens": "You execute mechanical Elixir refactor patterns through the bbox refactor primitives. Ground every operation via bbox_code_symbols and bbox_refactor_status before planning. Plan with deep_analysis=true. Compose primitives through bbox_refactor_run with a mix compile --warnings-as-errors --return-errors command step gated by repair. Emit bbox_note(kind=\"done\") with a one-line acceptance summary on completion. Refuse cleanly with bbox_note(kind=\"blocked\") and a concrete diagnostic when preconditions don't hold; never loop, never broaden charter."
}
```

Same allow/deny shape as the Rust/Java personas; `Bash`/`Write`/`Edit`
denied because all mutation goes through `bbox_refactor_run` command
steps and refactor-primitive atomic writes.

## Plan kinds

Each entry: kind name, semantic tier, what it does, atom(s) unblocked,
shape, refusals, deep-analysis report, nearest Rust/Java analogue.

### EX-G1. `split_elixir_clauses_by_tag` ★ keystone

**Semantic tier:** `indexed_hints`

**What:** Given a multi-clause `def`/`defp` where each clause pattern-matches
on a structured discriminator (typically a leading atom in a struct
pattern, optionally refined by guards or nested map/struct patterns),
partition the clauses across target submodules and generate a router.
The router preserves the original function signature, the exact clause
order within each bucket, and any guards verbatim; each submodule
exports one entry function (default name `run/N`, operator-overridable)
that owns its bucket.

**Head matcher contract.** The single-atom `tag_pattern: "%Op{kind: :TAG}"`
of the round-1 draft was too weak. The actual op_runtime.ex shape contains:

- Same-tag clauses with different `args` map patterns
  (`:author_edge` at `op_runtime.ex:570` and `:590`;
  `:emit_phase_evidence` at `:1674` and `:1680`).
- Multi-line heads with nested struct/map binding.
- Guards on otherwise-matching clauses (e.g.,
  `def run(data, %Op{kind: :foo, args: args}) when is_map(args)`).

To handle these, `head_matcher` is a structured shape:

```jsonc
"head_matcher": {
  "discriminators": [
    {
      "arg_index": 1,
      "binding": "%Op{kind: $TAG}",        // $TAG names the primary discriminator
      "primary": true
    },
    {
      "arg_index": 1,                       // see "cross-argument" note below
      "binding": "%Op{kind: $TAG, args: $ARGS_SHAPE}",
      "secondary": true                     // captures sub-shape per clause
    }
  ],
  "preserve_guards": "verbatim"             // see refusals
}
```

**Cross-argument discriminators (v1 constraint).** v1 requires all
`discriminators` to share the same `arg_index` (typically `1` — the
first non-`data` argument). Cross-argument discrimination (primary
tag in arg 1, refinement shape in arg 2) is NOT supported in v1
because the canonical Elixir shape pattern-matches structurally on a
single argument. Plan refuses with `error.bad_input(code=cross_arg_
discriminators)` if `discriminators` have differing `arg_index`. v2
may relax this.

The matcher returns, per clause: `{primary_tag, secondary_shape?, guard?,
clause_byte_range, clause_text}`. Clauses with the same `primary_tag` but
different `secondary_shape` form a **duplicate-tag group** (see below).

**Inputs:**
```
bbox_refactor_plan(
    kind="split_elixir_clauses_by_tag",
    source="apps/substrate/lib/substrate/op_runtime.ex",
    module_name="Substrate.OpRuntime",
    item_names=["run"],                           # function name(s)
    toml_entries={
      "arity": 2,
      "head_matcher": { ...as above... },
      "partition": {
        "Substrate.OpRuntime.Entity":   [":resolve_entity", ":invoke_projection",
                                         ":format_object"],
        "Substrate.OpRuntime.Explain":  [":explain_tool_exposure",
                                         ":explain_workflow_ratification",
                                         ":explain_impact_set"],
        "Substrate.OpRuntime.Author":   [":author_constellation",
                                         ":author_apply_composition_extension"]
      },
      "selection_mode": "exhaustive"             // see selection_mode below
                                                 // or "selected_only"
      "duplicate_tag_policy": "group_to_same_bucket",  // see below
      "target_dir": "apps/substrate/lib/substrate/op_runtime",
      "router_module": "Substrate.OpRuntime",     # rewritten parent
      "acknowledge_quote_in_moved": false,
      "acknowledge_use_at_scope": false,
      "acknowledge_defmacro_move": false,
      "acknowledge_unpreservable_guards": false    // see refusals
    },
    deep_analysis=true,
    project_dir="/abs/erlang-test"
)
```

**`selection_mode`:**
- `"exhaustive"` (default) — every primary tag in the source must appear
  in some partition bucket. Plan refuses with `error.unenumerated_tags`
  otherwise. Use when carving the function fully.
- `"selected_only"` — only the tags named in partition buckets are
  moved; unenumerated tags remain on the router unchanged. Use for
  incremental carve-outs ("pull out just the Entity concerns; leave
  the rest").

**Router body generation under `selection_mode` (round-2 fix,
deepseek M-R2-3).** The router function rewritten on the source
module is a hybrid in `selected_only` mode. Generation rules:

1. **Per-bucket dispatch wrapper.** For each tag assigned to a bucket,
   the router emits one wrapper clause:
   `def run(data, %Op{kind: <tag>} = op), do: <Target>.run(data, op)`
   placed at the original position of the first source clause
   matching that tag (preserves source-order semantics for
   pattern-match resolution).
2. **Unmoved clauses copied verbatim.** Unenumerated tags' clauses
   are copied verbatim from the original source position to the
   router function body, preserving order. Comments attached to these
   clauses move with them per EX-V6.
3. **Module attributes on the function** (`@doc`, `@spec`,
   `@dialyzer`, `@deprecated`) are attached to the function as a
   whole, not individual clauses. They stay on the router with one
   qualifier: a `@spec` whose argument types name moved tags (e.g.,
   `@spec run(data, %Op{kind: :resolve_entity | :foo | :bar}) ::
   ...`) is narrowed to drop the moved tags from the union — the
   router delegates them now, it doesn't handle them directly. Tags
   that remain on the router stay in the union. `@spec`s that don't
   name tags (generic shapes like `%Op{}` or `term()`) are unchanged.
   `@dialyzer` annotations are rewritten only when they name a
   specific clause-line `nowarn`/`no_return` form referring to a
   moved clause; otherwise unchanged.
4. **Per-clause attributes** (`@doc false` on a specific clause, rare
   but legal) move with their clause: copied verbatim with the
   unmoved clause body, or stripped from a moved-to-bucket clause
   (the bucket's `run/N` gets a fresh `@doc false` if and only if
   ALL clauses assigned to the bucket had it).
5. **Order preservation.** The router's final clause order matches
   the source's clause order: each clause position holds either its
   original body (unmoved) or a dispatch wrapper (moved). This keeps
   pattern-match precedence identical.

The same generation rules apply under `selection_mode: "exhaustive"`
— the only difference is that every clause becomes a dispatch
wrapper (no unmoved clauses remain). The hybrid form is therefore the
general case; exhaustive is a degenerate case of it.

**`duplicate_tag_policy`:**
- `"group_to_same_bucket"` (default) — all clauses sharing a primary tag
  must be assigned to one bucket. The bucket inherits the original
  clause order **verbatim, including any secondary-shape distinctions
  and guards** — the moved clauses look exactly as they did in the
  source, just relocated. This is the safe default for the canonical
  catch-all-then-specific shape (e.g., `:author_edge` at 570 with a
  specific `args` pattern and `:590` as a general fallback): moving
  both verbatim to one target preserves semantics with no router-side
  subkey discrimination. Plan refuses with
  `error.duplicate_tag_split_across_buckets` ONLY if a single primary
  tag's clauses are assigned to more than one bucket. Distinct
  secondary shapes within one bucket are accepted (round-2 fix:
  previous spec incorrectly refused this case).
- `"explicit_subkeys"` — used to intentionally split a duplicate-tag
  group across buckets. The partition value for a split duplicate-tag
  group is an object naming subkeys (e.g.,
  `":author_edge": {"by": "args.from", "buckets": {"EntityModule":
  ["edge_from_decision"], "AuxModule": ["edge_from_evidence"]}}`).
  The router matches the primary tag, then dispatches by the subkey.
  Only valid when the head_matcher's `secondary` discriminator captures
  the subkey shape. Implies operator has confirmed the split semantics.

**`captured_helpers` reachability strategy (round-2 fix):** Elixir code
commonly defeats static call-graph analysis via `apply/3`,
`Module.concat/2`, and capture-via-attribute patterns. The planner uses
a **two-tier explicit strategy**:

- **Tier 1, statically resolvable:** direct calls via `Foo.bar(...)` ,
  `bar(...)` (inferred to the same module), `&Foo.bar/2` /
  `&local_fn/2` captures. Tree-sitter / `Code.string_to_quoted` resolves
  these into the `captured_helpers` list with `confidence: "static"`.
- **Tier 2, dynamic-dispatch unresolved:** `apply(mod, fn_name, args)`,
  `Module.concat([prefix, suffix]).call(...)`,
  `Map.fetch!(@dispatch_map, key).(...)`,
  `String.to_atom("handle_#{kind}")`. These appear in a separate
  `dynamic_dispatch_unresolved` report list as
  `{clause_line, expression_excerpt}` with `confidence: "unresolved"`.
  The planner does NOT attempt to resolve them.

Plan refuses with
`error.bad_input(code=dynamic_dispatch_in_moved_clauses)` when any
moved clause body has unresolved dynamic dispatch, unless operator
passes `acknowledge_dynamic_dispatch: true` indicating they have
manually verified the unresolved sites either (a) don't dispatch to
helpers that need moving or (b) dispatch to helpers already in the
target module's compile graph.

This explicit strategy avoids the round-2 trap (deepseek M-R2-4)
where an optimistic approximation silently misses dependencies and a
conservative approximation flags every defp.

**Deep-analysis report:**
- `captured_helpers` — every statically-resolvable `defp` reachable
  from any moved clause, with `confidence: "static"` and a partition
  assignment (which targets need each helper).
- `dynamic_dispatch_unresolved` — Tier 2 sites as described above.
  Drives the dynamic-dispatch refusal.
- `shared_helpers` — helpers called from clauses in more than one
  partition; these stay in the router or move to a shared
  `Substrate.OpRuntime.Common` module (operator chooses via
  `toml_entries["shared_helper_target"]`).
- `captured_aliases` / `captured_imports` / `captured_requires` —
  module-level directives each target needs.
- `captured_attributes` — `@module_attr` references used by moved
  clauses.
- `unenumerated_tags` — primary tags seen in source clauses but not present
  in any partition bucket. Refuses when `selection_mode=exhaustive` and the
  list is non-empty.
- `duplicate_tag_groups` — `{tag: [{clause_line, secondary_shape, guard}, ...]}`
  for every primary tag with more than one clause. Under
  `group_to_same_bucket` all duplicate clauses for one tag must be
  assigned to one bucket (no split-across-buckets) but ARE moved
  verbatim including distinct secondary shapes and guards. Under
  `explicit_subkeys` the report drives the subkey routing.
- `clauses_with_guards` — `{clause_line: guard_expression}` for every
  guarded clause. The planner **always preserves guards verbatim** by
  copying the guard text to the corresponding submodule clause. When
  preservation fails (e.g., the guard references a `defp` that didn't
  move with the clause and isn't in the source's `imports` set), plan
  refuses with `error.guarded_clauses_require_review` including the
  failing guard's line and a one-line cause. Operator can bypass with
  `acknowledge_unpreservable_guards: true` (round-2 fix: renamed from
  `acknowledge_guard_drop` to reflect the intended truth-table —
  `true` means "I accept that the planner will drop guards it cannot
  preserve and proceed without them"; `false` (default) means
  "refuse if any guard cannot be preserved"). The planner NEVER drops
  guards under `false` and only drops guards under `true` for clauses
  where preservation actually failed; preservable guards are always
  copied verbatim regardless of flag value.
- `non_tag_clauses` — clauses whose head doesn't match the declared
  `head_matcher.discriminators[primary]` (e.g., a fallthrough
  `def run(_data, _op)` or a clause whose head binds a variable instead of
  an atom literal). Operator must assign each via
  `toml_entries["non_tag_assignments"]` or accept the default
  (`unassigned_clauses_target` → all stay on router).
- `external_callers` — sites outside the source module that call the
  function being split. Router preserves arity/name so external callers
  unaffected, but the report lists them for visual audit. Skipped when
  the source function is `defp` (no external callers possible).

**Refusals:**
- `error.unenumerated_tags` — any primary tag in source missing from
  `partition` under `selection_mode=exhaustive`.
- `error.duplicate_tag_split_across_buckets` — a primary tag's clauses are
  assigned to more than one bucket under `group_to_same_bucket`.
- `error.guarded_clauses_require_review` — guarded clauses present and at
  least one guard cannot be preserved verbatim on the destination
  submodule; refused unless `acknowledge_unpreservable_guards: true`.
  Preservable guards are always copied; the refusal fires only on the
  un-preservable subset.
- `error.tag_not_atom_literal` — a clause head matches on a non-atom-literal
  pattern (e.g., `:tag = atom_var` binding); these can't be statically
  partitioned. Operator must rewrite or exclude.
- `error.bad_input(code=quote_in_moved_clauses)` — moved clause body
  contains `unquote`/`quote` blocks. Refused unless
  `acknowledge_quote_in_moved: true`.
- `error.bad_input(code=use_at_scope)` — source module has `use Foo` at
  module scope whose expansion affects the moved clauses' visible API.
  Refused unless `acknowledge_use_at_scope: true`.
- `error.bad_input(code=defmacro_in_moved)` — moved item is a `defmacro`.
  Refused unless `acknowledge_defmacro_move: true`.

**Atoms unblocked:** `elixir-shatter-dispatch-table`,
`elixir-split-genserver` (composes this for the inner `defp dispatch/1`
clauses).

**Nearest existing:** None. The Rust `rust_match_arm_to_strategy` (RX-P1)
generates per-variant strategy modules from a match expression on an enum;
the Elixir shape is structurally different (function-head dispatch, not
expression-level match), the discriminator is an atom in a struct pattern
(not an enum variant), and clause-order semantics, guards, and same-tag
duplicates are all first-class concerns that have no analogue in RX-P1.

### EX-G2. `extract_elixir_module`

**Semantic tier:** `syntax_only`

**What:** Move named `def` / `defp` / `defmacro` items from a source module
to a new module file. Rewrites `alias`/`import` in source to point at the
new module. Refuses on items inside `quote do ... end` or items injected by
`use Foo` unless the matching split acknowledgment is set (EX-V1).

**Inputs:** `source`, `target` (new module file), `target_module_name`,
`item_names` (list of `name/arity` strings), `move_attributes`
(module attributes to copy), `acknowledge_quote_in_moved`,
`acknowledge_use_at_scope`, `acknowledge_defmacro_move`, `apply`.

**Deep-analysis report:** same shape as Java `extract_java_class`
(`captured_variables`, `external_calls`, `inherited_dependencies` adapted
to Elixir — function-level captures are just module aliases since Elixir
has no inheritance).

**Refusals:**
- `error.bad_input(code=use_at_scope)` — source module has `use Foo` from
  outside the project unless `acknowledge_use_at_scope: true`.
- `error.bad_input(code=quote_in_moved)` — moved item nests inside a
  `quote do ... end` block; refused unless `acknowledge_quote_in_moved: true`.
- `error.bad_input(code=defmacro_move)` — moved item is a `defmacro`;
  refused unless `acknowledge_defmacro_move: true`.

**Nearest existing:** `extract_rust_items`, `move_java_field`.

### EX-G3. `extract_genserver_callback_group` ★

**Semantic tier:** `indexed_hints`

**What:** Pull a cohesive subset of GenServer callbacks — client-API
definitions plus their server-side handler clauses — from a source
GenServer into a new GenServer module. Rewrites the source GenServer
to delegate via `GenServer.call(SecondaryGenServer, ...)` or rewrite
callers, atomically wiring the supervisor child spec.

Two source shapes are supported, declared via `dispatch_pattern`:

- **`single_dispatch_fn`** — the source has a single generic
  `handle_call(request, from, state)` that delegates to a private
  `defp dispatch(request)` with one clause per message. The triplet
  unit is `{client_api, dispatch_clause}` (the generic `handle_call`
  stays on the source). Matches `admin_endpoint.ex:145-198`.
- **`per_message_handle_call`** — the traditional GenServer shape,
  one `handle_call({:msg, ...}, from, state)` clause per message,
  no inner dispatch function. The triplet unit is `{client_api,
  handle_call_clause}`. The handle_call clause moves verbatim.

Mixed sources (some messages dispatched, some direct) are accepted by
passing `dispatch_pattern: "mixed"` and a per-name annotation; the
planner reports per-name pattern in the deep analysis and errors when
the operator's annotation disagrees.

Round-1 review hit: previously called `extract_genserver_call_triplet`,
which implicitly required the `admin_endpoint.ex` single-dispatch shape
and refused on `per_message_handle_call` GenServers with a misleading
`error.incomplete_triplet`. The renamed kind explicitly models both.

**Inputs:**
```
bbox_refactor_plan(
    kind="extract_genserver_callback_group",
    source="apps/substrate/lib/substrate/admin_endpoint.ex",
    target="apps/substrate/lib/substrate/admin_endpoint/checkpoint_admin.ex",
    target_module_name="Substrate.AdminEndpoint.CheckpointAdmin",
    item_names=["verify_checkpoint", "canonical_hash", "pin_known_good",
                "rollback_to", "force_restart"],
    toml_entries={
      "dispatch_pattern": "single_dispatch_fn",   # or "per_message_handle_call" / "mixed"
      "client_api_strategy": "delegate",          # or "rewrite_callers"
      "supervisor_module": "Substrate.Application",
      "supervisor_child_id": "checkpoint_admin"
    },
    deep_analysis=true,
    project_dir="/abs/erlang-test"
)
```

Constraint: `dispatch_pattern: "per_message_handle_call"` is incompatible
with `client_api_strategy: "delegate"` — delegate routing needs a
single client-API → message-tag indirection that direct-handle_call
GenServers don't provide. Refused upfront with
`error.bad_input(code=delegate_requires_dispatch_fn)`.

**Async / support callbacks (round-2 fix).** Round-2 review hit
(codex M-R2-3): the original EX-G3 modeled only `handle_call`-related
triplets but ignored `handle_info/2` and `handle_continue/2`
callbacks that participate in the same request lifecycle. The
admin_endpoint.ex async path (`admin_endpoint.ex:156` and `:169`) uses
two generic `handle_info({ref, reply}, state)` and
`handle_info({:DOWN, ref, ...}, state)` clauses plus a `pending: %{},
refs: %{}` state shape to manage Task replies; these MUST move with
the async client APIs or be acknowledged as staying behind.

Plan accepts `support_callbacks` input:

```jsonc
"support_callbacks": [
  {
    "callback": "handle_info/2",
    "pattern_match": "{ref, reply}",
    "disposition": "move_with_async_group"
  },
  {
    "callback": "handle_info/2",
    "pattern_match": "{:DOWN, ref, :process, _pid, reason}",
    "disposition": "move_with_async_group"
  }
]
```

When the planner's `async_classification` flags any moved client API
as async, the plan refuses with
`error.bad_input(code=async_support_undeclared)` unless the operator
declares the support callbacks. Disposition `"move_with_async_group"`
moves the callback verbatim into the target along with the state
fields it touches; `"keep_on_source"` requires the operator to
explicitly acknowledge that the async lifecycle is split (typically a
mistake, hence the explicit choice).

**Deep-analysis report:**
- `triplet_completeness` — for each requested name and the resolved
  `dispatch_pattern`: under `single_dispatch_fn` reports
  `{client_api: bool, dispatch_clause: bool}`; under
  `per_message_handle_call` reports
  `{client_api: bool, handle_call_clause: bool}`. Plan refuses if any
  required component is missing.
- `detected_dispatch_pattern` — per name, the planner's inference
  (used for `dispatch_pattern: "mixed"` cross-check).
- `state_field_touches_report` — for each moved callback, which state
  fields the planner can identify as read/written by **syntactic
  catalog only**. **Advisory, NOT a refusal gate** (round-2 fix:
  deepseek C-R2-1 demonstrated this analysis is fundamentally
  unsolvable at the `indexed_hints` tier because Elixir state access
  has no canonical syntactic form). The catalog the planner
  recognizes is:
  - `state.field` and `state[:field]` direct access
  - `%{state | field: ...}` struct update
  - `%{field: x} = state` pattern destructure
  - `Map.get(state, :field, _)`, `Map.put(state, :field, _)`,
    `Map.fetch!(state, :field)`, `Map.update!(state, :field, _)`
  - `put_in(state.field, _)` / `update_in` (resolved through the
    `Access` macro expansion the planner can identify
    syntactically)
  Forms outside this catalog (helper-mediated access, dynamic atoms,
  Keyword-list state, etc.) appear in `state_field_touches_unresolved`
  as a list of `{clause_line, expression_excerpt}` items for operator
  review. The planner does NOT refuse on shared mutable state
  detected by this catalog — too many false negatives/positives. The
  operator MUST review `state_field_touches_unresolved` and either
  refactor to a recognized form first or pass
  `acknowledge_shared_state: true` indicating they have manually
  reviewed the analysis limitations.
- `async_classification` — for each moved name, whether it appears in
  the source's `async_request?/1` (or equivalent) predicate. Drives
  the `async_support_undeclared` refusal above.
- `supervisor_wiring` — the supervisor child spec edits required to
  start the new GenServer. The plan includes these as a separate
  FileEdit on the supervisor module.

**Refusals:**
- `error.bad_input(code=incomplete_triplet)` — a requested name has
  only one or two of the required components.
- `error.bad_input(code=async_support_undeclared)` — async-classified
  client API requested for move without matching
  `support_callbacks` declarations.
- `error.bad_input(code=supervisor_not_found)` — `supervisor_module`
  doesn't contain a `children/0` or `init/1` returning a
  `Supervisor.init` shape.
- `error.bad_input(code=acknowledge_shared_state_required)` — the
  state-field analysis surfaced any
  `state_field_touches_unresolved` entries and operator did not pass
  `acknowledge_shared_state: true`. This is the only state-related
  refusal; advisory state-field overlaps within the recognized
  catalog do NOT refuse on their own.

**Atoms unblocked:** `elixir-split-genserver`.

**Nearest existing:** Composite of `extract_java_class` (for the move) +
implicit caller rewrite. Java's `extract_java_class` handles
state-field analysis at parity because Java field access has one
canonical syntax (`this.field`); Elixir GenServer state access has no
canonical form and the EX-G3 analysis is honestly advisory.

### EX-G4. `add_elixir_facade_delegations`

**Semantic tier:** `syntax_only`

**What:** Given a facade module and a backing module, generate the
`defdelegate` set so the facade exposes the backing module's public surface.
Maintenance tool: regenerate when a backing module gains or loses functions.

**Inputs:** `source` (facade), `target` (backing module), `name_filter`
(regex or explicit allow list), `arity_filter`, `as_renames` (map of
`backing_name → facade_name`), `keep_existing: bool`.

**Deep-analysis report:** `added`, `removed`, `kept_existing`, `renames`.

**Refusals:** None that fail closed; the operation is purely additive.
`name_filter` empty + `keep_existing: false` is a warning (would drop all
delegations).

**Atoms unblocked:** `elixir-facade-wire`.

**Nearest existing:** `add_java_delegate_field` is the spiritual analogue
but Java delegate fields are at instance scope; Elixir `defdelegate` is at
module scope and bulkier.

### EX-G5. `rename_elixir_symbol`

**Semantic tier:** `lsp_verified` for supported symbol kinds;
`error.lsp_unavailable` otherwise. **No syntactic fallback for
unsupported kinds** — see capability matrix.

**What:** Rename an Elixir symbol via the language server when the
backend supports the symbol kind. Renames carry hot-code-reload
implications (named registries) that only the LSP can verify safely
even when the LSP itself is also limited.

**Round-1 review hit (codex verified):** elixir-ls's public feature
list does NOT include `textDocument/rename`. The upstream tree has
protocol structs for rename but no provider implementation under
`providers/`; the execute-command handlers cover pipe/macro/spec
manipulation but not rename. lexical's rename support is similarly
limited as of 2026-05. The honest position: this kind is a **probe-
or-refuse** primitive — the planner asks the LSP whether the symbol at
the named position is renameable, refuses with `error.lsp_unavailable`
or `error.symbol_not_renameable` when not, and does NOT silently
fall back to a syntactic substitution.

**Capability matrix (refresh per LSP version at plan time):**

| Symbol kind                       | elixir-ls (2026-05) | lexical (2026-05) | Plan kind v1 |
|-----------------------------------|---------------------|-------------------|--------------|
| In-file local variable            | partial             | partial           | refuses      |
| Module alias                      | partial             | partial           | refuses      |
| Public def cross-file             | unsupported         | unsupported       | refuses      |
| GenServer module name             | unsupported         | unsupported       | refuses      |
| `@behaviour` callback             | unsupported         | unsupported       | refuses      |
| Module attribute name             | unsupported         | unsupported       | refuses      |

In v1 every entry refuses. The plan kind exists as a structured refusal
surface so callers don't reinvent syntactic rename (which would
unsafely miss `name: __MODULE__` registrations, supervisor child specs,
and `@behaviour` references). Operators wanting v1 renames perform them
manually via editor refactor or `mix format`'s
`Mix.Tasks.Format.Renames` task when available.

When elixir-ls or lexical ship a working `textDocument/rename`
provider, this matrix flips and the kind starts honoring renames
incrementally per symbol kind; the matrix is the authority.

**Inputs:** `source`, `position: {line, column}` (or `item_name`
resolved via `bbox_code_symbols`), `new_name`, `project_dir`,
`expected_symbol_kind` (must match the matrix's `Plan kind v1` to
proceed beyond probe).

**Deep-analysis report:** `lsp_probe_response` (raw
`textDocument/prepareRename` result), `references_found_via_probe` count
by file, `process_registries_in_source` (any `name: __MODULE__` /
`name: ModName` matches the planner detected via tree-sitter
independently of LSP, surfaced as advisory), `supervisor_specs_in_source`,
`@behaviour_refs_in_source`.

**Refusals:**
- `error.lsp_unavailable` — LSP not running or RPC error.
- `error.symbol_not_renameable` — capability matrix marks this symbol
  kind as refuse-in-v1.
- `error.bad_input(code=symbol_kind_mismatch)` — `expected_symbol_kind`
  doesn't match what the planner finds at `position`.

**Atoms unblocked:** `elixir-rename-symbol` ships as analysis-only in
v1 (reports what would be renamed; refuses to apply). The kind is
ready for v2 once LSP support lands.

**Nearest existing:** `rust_lsp_rename` (which actually fires through
rust-analyzer's robust `textDocument/rename`), `rename_java_symbol`
(jdtls-backed). The Elixir kind is the LSP analogue *structurally*
but honest about backend gaps.

### EX-G6. `elixir_organize_aliases`

**Semantic tier:** `syntax_only`

**What:** Sort, dedupe, and collapse `alias` / `import` / `require` / `use`
directives at the top of an Elixir module. Collapse to grouped forms when
multiple aliases share a parent
(`alias Foo.A; alias Foo.B; alias Foo.C` →
`alias Foo.{A, B, C}`). Expand grouped forms when only one member survives.
Sort alphabetically within each directive family. `use` stays in textual
order (use can have side effects on subsequent directives).

**Inputs:** `source`, `apply`. No deep_analysis report — pure formatting.

**Refusals:**
- `error.bad_input(code=directive_in_macro)` — directive nests inside a
  `quote do ... end` or `if @some_attr do alias ... end` conditional;
  reorder would change semantics.

**Atoms unblocked:** `elixir-organize-aliases`.

**Nearest existing:** `rust_organize_imports`, `java_lsp_organize_imports`.

### EX-G7. `elixir_module_dependency_analysis`

**Semantic tier:** `indexed_hints`

**What:** Build an inter-module call graph for a target file or directory.
Uses `mix xref graph --format=dot` plus per-source-file `Code.required_files`
to determine compile-time vs runtime dependencies. Output is analysis-only
(no FileEdits).

**Output:**
```
{
  "nodes": [{"module": "Substrate.AdminEndpoint", "loc": 2796, "publics": 53}, ...],
  "edges": [{"from": "Substrate.AdminEndpoint", "to": "Substrate.Graph",
             "kind": "runtime", "call_count": 14}, ...],
  "cycles": [["Substrate.A", "Substrate.B", "Substrate.A"]],
  "fan_in_max": {"Substrate.Graph.Store": 42},
  "compile_time_edges": [...]
}
```

**Atoms unblocked:** `elixir-module-dependency-graph` (cost_class=cheap,
analysis-only).

**Nearest existing:** `rust_top_level_dependency_analysis`,
`java_class_dependency_analysis`.

### EX-G8. `elixir_public_api_guard`

**Semantic tier:** `indexed_hints`

**What:** Inventory `def`s reachable from a marked facade module (or
`@moduledoc false`-excluded set), report the delta against a proposed plan.
Same operator-authority shape as Rust/Java: `acknowledge_public_api_change`
must be passed by the operator and never defaulted by a calling atom
(see invariant **EX-V5**).

**Inputs:** `source` (facade or directory), `proposed_changes` (plan step
refs), `facade_modules` (list of module names treated as the public
surface boundary).

**Output:** `public_items_touched`, `public_api_delta_summary`,
`facade_re_exports_affected`, `advisory_severity`.

**Atoms unblocked:** `elixir-public-api-guard`.

**Nearest existing:** `rust_public_api_guard`, `java_public_api_guard`.

### EX-G9. `extract_elixir_behaviour`

**Semantic tier:** `indexed_hints`

**What:** Lift a function set on a module into a `@behaviour` module with
`@callback` declarations. Adds `@behaviour Module` to the source and `@impl
Module` on each lifted def. Optionally inserts a default implementation
module (`use Behaviour, default: SomeOtherImpl`).

**Inputs:** `source`, `target` (new behaviour module file),
`behaviour_module_name`, `item_names`, `generate_default_impl: bool`,
`apply`.

**Deep-analysis report:**
- `callback_signatures` — typespec inferred per callback (best-effort from
  existing `@spec`s on the source; otherwise generic `any()`).
- `callsite_warnings` — sites that call the lifted functions; will continue
  to work unchanged but flagged for visual audit.
- `mfa_capture_warnings` — callsites that capture the function as an
  anonymous fn (`&Foo.bar/2`); behaviour adoption doesn't break these but
  the captured ref pin to the original module.

**Refusals:**
- `error.bad_input(code=anonymous_fn_signature)` — a lifted def takes an
  anonymous fn parameter the planner can't write a `@callback` for
  cleanly. Operator must rewrite to MFA tuples or pass
  `acknowledge_anonymous_fn_callbacks: true`.

**Atoms unblocked:** `elixir-extract-behaviour`.

**Nearest existing:** `extract_rust_trait`, `extract_java_interface`.

### EX-G10. `elixir_codegen_audit`

**Semantic tier:** `syntax_only` (analysis-only; emits snapshots, not edits)

**What:** For a module containing `quote do defmodule unquote(name) do ...
end end` (or any compile-time codegen pattern), expand the codegen for a
sample input and write a pinned snapshot file. No edits to source. The
snapshot serves as a regression artifact for code review.

**Inputs:** `source`, `sample_inputs` (list of inputs to feed to the
codegen function), `snapshot_dir` (e.g. `priv/codegen_snapshots`).

**Output:** snapshot file paths per sample input. The compiled AST is
written as Elixir source via `Macro.to_string/1` so it's diffable.

**Atoms unblocked:** `elixir-codegen-audit` (cost_class=cheap).

**Nearest existing:** None directly. Rust `cargo expand` is the spiritual
analogue but is a toolchain command, not a refactor primitive. The audit
here writes regression-pinnable artifacts.

### EX-G11. `elixir_compile_fix_round`

**Semantic tier:** `syntax_only` (edit proposals from structured diagnostics)

**What:** Parse `mix compile --warnings-as-errors --return-errors`
diagnostics into actionable edit proposals. Composes inside
`bbox_refactor_run` with `on_failure="continue_for_repair"`. Classification
mirrors `rust_compile_fix_round`:

- Undefined alias / module → propose `alias`/`require` addition.
- Function arity mismatch → propose call-site rewrite to the new arity
  when there's exactly one matching def.
- Unused alias / unused import → propose removal.
- Unknown function in defmodule → propose adjacent-stub or remove.

**Inputs:** `diagnostics_ref` (compound-run reference to a captured
diagnostics step; see `refactor-compound-runs.md`).

**Output:** proposed FileEdits + `unresolved_diagnostics`.

**Atoms unblocked:** any atom composing `bbox_refactor_run` with a
`mix compile` step.

**Nearest existing:** `rust_compile_fix_round`.

### EX-G12. `elixir_credo_fix_round`

**Semantic tier:** `syntax_only`

**What:** Apply machine-applicable Credo lint fixes (style and design
issues, the subset Credo flags as auto-fixable). Like clippy's
`MachineApplicable` lints, only the safe subset becomes edit proposals.

**Inputs:** `diagnostics_ref` (a captured `mix credo --format=json` step).

**Output:** FileEdits per fixable lint, warnings per non-fixable.

**Atoms unblocked:** `elixir-credo-fix-round`,
`elixir-auto-lint-fix`.

**Nearest existing:** Rust gap inventory G13 `rust_clippy_fix_round`.

### EX-G13. `elixir_dialyzer_attribution`

**Semantic tier:** `indexed_hints`

**What:** Map dialyzer warnings to defs, propose `@spec` insertions or
contract-narrowing edits. Dialyzer is success-typing — its warnings are
"this can never succeed" rather than "this might fail", so most warnings
suggest a contract is wrong, not the code. The plan kind classifies:

- `no_return` on a function that does return → narrow `@spec`.
- `extra_range` on a function `@spec` → narrow return type.
- `call_to_missing` → propose `alias`/`require` or rename to a defined
  function.
- `pattern_match_cov` → propose adding a missing clause or removing an
  unreachable one.

**Inputs:** `diagnostics_ref` (a captured `mix dialyzer --format=short`
step).

**Output:** FileEdits + `unactionable_warnings` (most dialyzer warnings
are advisory, not fixable).

**Atoms unblocked:** `elixir-dialyzer-attribution`.

**Nearest existing:** None at parity. Rust has no success-typing pass;
Java has `@Nullable`/`@NotNull` but no equivalent JSON pipeline. This is
BEAM-specific.

### EX-G14. `elixir_genserver_state_audit`

**Semantic tier:** `indexed_hints`

**What:** Analyze a GenServer module and produce the inferred state schema
(field set, per-field types from `@spec` or default-value inference) plus
a per-callback field-read/write map. Analysis-only; precondition for
`extract_genserver_callback_group` (formerly `extract_genserver_call_triplet`).

**Inputs:** `source`, `genserver_module_name`.

**Output:**
```
{
  "state_fields": {"pending": "map()", "refs": "map()", ...},
  "per_callback": {
    "handle_call/3:verify_checkpoint":  {"reads": ["refs"], "writes": ["refs", "pending"]},
    "handle_info/2:DOWN":               {"reads": ["refs", "pending"], "writes": [...]}
  },
  "init_initializers": {"pending": "%{}", "refs": "%{}"},
  "supervisor_child_specs": [{"id": "...", "start": {...}}]
}
```

**Atoms unblocked:** `elixir-genserver-state-audit` (cost_class=cheap).

**Nearest existing:** Closer to Java's class-level capture analysis than
to anything Rust ships, but specialized for GenServer state.

### EX-G15. `elixir_test_fixture_extract`

**Semantic tier:** `syntax_only`

**What:** Identify repeated `setup` / `setup_all` blocks across `*_test.exs`
files, lift the common ones into a fixture module exposed via
`use Substrate.TestFixtures, :graph` (or operator-named). Updates each
test file's `setup` to call the fixture.

**Inputs:** `source_dir`, `target_module_name`, `fixture_name`,
`min_duplicates: int` (default 3), `apply`.

**Refusals:**
- `error.bad_input(code=setup_references_module_scope)` — extracted setup
  body references `@module_attr` of the test module. Operator must inline
  or pass `acknowledge_attribute_scope: true`.

**Atoms unblocked:** `elixir-test-fixture-extract`.

**Nearest existing:** Conceptually similar to the Rust `rust-test-island-extract`
atom but the unit moves up (setup logic across files) not down (test block
inside a file).

### EX-G16. `inline_elixir_module`

**Semantic tier:** `syntax_only`

**What:** Reverse of `extract_elixir_module`. Take a small module file and
inline its content into its caller (or a designated target) as a private
section. Refuses on modules with `@behaviour` directives, `defstruct`s, or
nested modules.

**Inputs:** `source` (module to inline), `target` (where to inline).

**Refusals:**
- `error.bad_input(code=module_is_struct_carrier)` — source defines
  `defstruct`; inline would break struct-type identity.
- `error.bad_input(code=module_is_behaviour)` — source has `@behaviour`
  directive; inline would lose the contract.
- `error.bad_input(code=module_has_compile_callbacks)` — source has
  `@before_compile` or `@after_compile`; inline changes when those fire.

**Atoms unblocked:** `elixir-inline-module`.

**Nearest existing:** Rust gap inventory G17 `rust_inline_module`.

### EX-G17. `elixir_pipe_chain_extract`

**Semantic tier:** `indexed_hints` (LSP-augmented when available)

**What:** Extract a contiguous subsequence of a `|>` pipe chain into a
named private function, then replace the subsequence with a single
piped call. The inverse operation (inline a piped private function
back into the chain) shares the plan kind under `direction: "inline"`.

elixir-ls's `manipulatePipes` execute-command performs to-pipe and
from-pipe rewrites on a single call, NOT extraction. Round-2 review
hit (deepseek M-R2-6) correctly flagged the original wording as a
category error. The plan kind's relationship to `manipulatePipes` is
narrow: when the operator's extraction operates on a non-pipe
expression (e.g., extracting a step expressed as `b(a(x))` rather than
`x |> a() |> b()`), the planner can use `manipulatePipes` to
normalize the expression to pipe form first; from there the
plan-kind-owned extraction logic operates on the normalized chain.
This pre-normalization is optional; when the source is already in
pipe form, `manipulatePipes` is not invoked. The chain parsing and
extraction logic itself live in the writable AST lane and never
depend on elixir-ls.

**Inputs:** `source`, `position: {line, column}` (anchor inside the
chain), `extract_range: {start_line_col, end_line_col}`,
`extracted_function_name`, `direction: "extract" | "inline"`,
`visibility: "def" | "defp"` (default `defp`), `apply`.

**Deep-analysis report:** `chain_steps` (full chain pretty-printed),
`extracted_subsequence`, `captured_variables` (variables referenced
inside the subsequence that come from outside the chain),
`type_inference_notes` (best-effort: pipe input type → output type
chain inference from elixir-ls hovers when available, otherwise
`unknown`).

**Refusals:**
- `error.bad_input(code=range_breaks_chain)` — the extract range spans
  the entry point of the chain (the leftmost expression) without
  including it, leaving the chain head ambiguous.
- `error.bad_input(code=captured_self_reference)` — a captured variable
  is `__MODULE__` or another compile-time-only binding that can't
  cross a function boundary cleanly.

**Atoms unblocked:** `elixir-pipe-chain-extract`.

**Nearest existing:** No direct Rust/Java analogue. Elixir's `|>` is
the canonical composition primitive and its refactor surface is unique.

### EX-G18. `elixir_with_clause_extract`

**Semantic tier:** `indexed_hints`

**What:** Extract a contiguous prefix or suffix of a `with` block's
clauses into a separate function. The extracted function returns
`{:ok, intermediate} | {:error, reason}` so the parent `with`'s
failure-arm semantics are preserved. The `else` block stays with the
parent unless the extracted prefix is the only producer of every
error pattern, in which case `else` arms matching only extracted
errors are moved with the prefix.

**Inputs:** `source`, `with_block_position`, `extract_clauses:
{start_clause_idx, end_clause_idx}`, `extracted_function_name`,
`apply`.

**Deep-analysis report:** `clause_count`, `extracted_clauses`,
`captured_bindings_in_remaining`, `else_arm_assignment` (which `else`
arms move with the extract), `else_arm_unassignable` (arms matching
patterns the planner can't statically attribute to extract-vs-remainder
— refuses if non-empty without `acknowledge_else_arm_residue: true`).

**Refusals:**
- `error.bad_input(code=else_arm_residue)` — `else` arms cannot be
  cleanly attributed; operator must split manually or acknowledge.
- `error.bad_input(code=extract_breaks_pattern_chain)` — a binding
  introduced in the extract is referenced in the remaining clauses but
  the planner cannot pass it cleanly (e.g., binding is a pattern
  match that destructures further in remainder).

**Atoms unblocked:** `elixir-with-clause-extract`.

**Nearest existing:** No direct Rust/Java analogue. Monadic
composition is Elixir-idiomatic.

### EX-G19. `elixir_move_module_across_apps`

**Semantic tier:** `indexed_hints`

**What:** Move an Elixir module from one umbrella `apps/<src>/` to
another `apps/<dst>/`. Atomically rewrites the module file location,
updates all `alias`/`import`/`require` references project-wide, and —
critically — updates `apps/<src>/mix.exs` to drop the destination app
from `:deps` if no other module remains that requires it, OR adds the
source-app dependency to `apps/<dst>/mix.exs` if the moved module
depends on modules still in source-app.

**Inputs:** `source` (module file under apps/), `target_app` (e.g.,
`apps/witness`), `target_path_in_app` (default mirrors source path),
`acknowledge_app_boundary_crossing: bool`, `apply`.

**Deep-analysis report:** `cross_app_dependencies` (modules in
target_app that the moved module depends on, and modules in source-app
that the moved module depended on), `mix_exs_edits` (proposed
`:deps` modifications for both apps), `config_references` —
canonicalized list of every reference to the moved module's name
found across `config/*.exs`, `apps/*/config/*.exs`, and `rel/*.exs`
(release configs). Includes both `config :app, ModuleName, ...`
forms and `config :app, key: ModuleName` value forms; both rewritten
to point at the new module name in their original app.

**Refusals:**
- `error.bad_input(code=cyclical_app_dependency)` — moving the module
  would introduce an `apps/<a>` → `apps/<b>` → `apps/<a>` dependency
  cycle.
- `error.bad_input(code=mix_exs_unparseable)` — the target or source
  app's `mix.exs` has dynamic deps logic the planner can't safely
  edit (e.g., `deps:` computed from `Mix.env()` or filesystem
  conditions, `deps()` defined as a function with case branches).
  Refused unless operator passes
  `acknowledge_app_boundary_crossing: true` AND manually edits mix.exs
  before re-running.
- `error.bad_input(code=config_dynamic)` — `config/*.exs` references
  the moved module via dynamic atom synthesis
  (`Module.concat([prefix, suffix])` in a config call). Planner
  cannot safely rewrite. Operator must manually update config first.

**Atoms unblocked:** `elixir-move-across-apps`.

**Nearest existing:** No Rust/Java analogue. Mix umbrella apps are
Elixir-specific.

## Invariants

Numbered EX-V* to mirror the Rust RX-V* scheme.

### EX-V1. Operator-authority opt-out flags

The macro-expansion concern decomposes into three distinct phenomena with
different risk and different ownership implications. Round-1 review hit:
a single `acknowledge_macro_expansion` flag collapses to always-on in
macro-heavy Elixir code, defeating audit. The flag is split into three
narrower acknowledgments:

- **`acknowledge_quote_in_moved`** — moved code body contains
  `unquote`/`quote` blocks. Lowest risk; the planner can see the
  pre-expansion form.
- **`acknowledge_use_at_scope`** — source module has `use Foo` at module
  scope whose `__using__` expansion injects callbacks/imports/macros that
  affect the moved API. Highest risk; the planner cannot see what use
  injects. Always required for GenServer / Phoenix / Ecto modules.
- **`acknowledge_defmacro_move`** — moved item is itself a `defmacro`.
  Medium risk; callers expand at compile time and the macro's behavior
  must be preserved across the move.

Other operator-authority flags:

- `acknowledge_shared_state` (`extract_genserver_callback_group`)
- `acknowledge_anonymous_fn_callbacks` (`extract_elixir_behaviour`)
- `acknowledge_attribute_scope` (`elixir_test_fixture_extract`)
- `acknowledge_describe_context` (`elixir_test_fixture_extract` — see
  EX-G15)
- `acknowledge_unpreservable_guards` (`split_elixir_clauses_by_tag`)
  — `false` (default): refuse the plan if any clause guard cannot be
  preserved verbatim. `true`: accept that the planner will drop the
  un-preservable subset of guards and proceed; preservable guards are
  always copied regardless. Renamed from `acknowledge_guard_drop` in
  round 2 because the original name suggested guards are always
  dropped; in fact the planner preserves them whenever possible and
  only the failing subset is at risk under `true`.
- `acknowledge_dynamic_dispatch` (`split_elixir_clauses_by_tag`) —
  operator confirms they have manually verified that
  `dynamic_dispatch_unresolved` sites in moved clauses don't dispatch
  to helpers that need moving with the clauses, OR that those
  helpers are already in the target module's compile graph. Required
  when any moved clause has unresolved dynamic dispatch (see EX-G1
  reachability strategy).
- `acknowledge_public_api_change` (any plan kind touching public surface)

Atomic agents from `../refactor-agents.md` MAY pass these flags through
from operator-supplied inputs but MUST NOT default them, MUST NOT infer
them from context, and MUST NOT set them silently after seeing a refusal.
Plan responses carry `operator_opt_outs_used` listing flags actually
consumed (named individually, not collapsed); this field lives on the
durable RefactorPlan.

Mirrors RX-V1 verbatim. Same enforcement story.

### EX-V2. Mix-only command allowlist for atom dispatches

`bbox_refactor_run` invocations dispatched from atomic refactor agents
are restricted to:

- `mix compile` (any args; read-only)
- `mix test` (any args; read-only on source, may mutate `_build/`)
- `mix credo` (any args; read-only)
- `mix dialyzer` (any args; may mutate `_build/dev/dialyxir/` PLT)
- `mix format --check-formatted` (any args; **read-only**, no
  `touches` requirement — the runner has no rollback obligation
  because the command mutates nothing)
- `mix format` without `--check-formatted` (mutating; `touches` MUST
  be declared so the runner can snapshot/rollback)
- `mix xref` (any args; read-only)

Any other command in an atom-dispatched run is a prompt-discipline
violation; the atom must refuse to compose it. Mutating commands not
in the allowlist must declare `touches` so the runner can
snapshot/rollback.

Mirrors RX-V2 with the read-only/mutating split made explicit (the
Rust V2 spec is implicit on this distinction; the Elixir spec is
explicit because `mix format` ships both read-only and mutating modes
under one binary).

### EX-V3. LSP-backed plan kinds fail closed

`rename_elixir_symbol` (and any future LSP-backed Elixir kind) requires
an active elixir-ls session. When the LSP is unavailable (binary
missing, init timeout, crashed mid-run), the plan kind MUST fail closed
with `error.lsp_unavailable` and the underlying cause. It MUST NOT
silently downgrade to a syntactic-rename approximation, because callers
chose the LSP-backed kind specifically for `semantic_status=lsp_verified`.

Hot-code-reload, named registries, and `@behaviour` reachability all
require semantic verification. Syntactic substitution is unsafe.

Mirrors RX-V3.

### EX-V4. GenServer module rename atomicity

Renaming a GenServer module (via `rename_elixir_symbol` where the symbol
is a module that has `use GenServer` at top scope) must atomically
rewrite:

- The `defmodule` line.
- Every `name: __MODULE__` / `name: <ModName>` callsite inside the
  module.
- Every reference in the application supervisor's `children/0` (or the
  function returning the supervisor's child specs).
- Every `alias` in client code.
- Every `start_link/1` callsite that names the GenServer explicitly.

The plan kind owns all five edits as one transactional unit. The LSP
identifies the surfaces; the plan kind ensures atomicity. Splitting
across plans is a contract violation — half-renamed GenServers crash at
boot.

No Rust/Java parallel; named-process registration is BEAM-specific.

### EX-V5. Public-API guard is advisory, not authority

`elixir_public_api_guard` reports the public-API delta but does not
decide whether the change is acceptable. Per RX-V1's operator-authority
contract, the calling atom passes `acknowledge_public_api_change: true`
on the operator's authority; the guard's role is to make the delta
visible, not to gate it.

### EX-V6. Writable-lane round-trip preservation

Every plan kind that emits FileEdits MUST go through the writable AST
lane (`Code.string_to_quoted_with_comments!/2` + comment-preserving
serializer) AND MUST verify round-trip identity as part of the apply
step. Round-2 review hit (deepseek M-R2-1, codex major): "ignoring
`:line` and `:column`" is under-specified — Elixir's quoted-form
metadata has 7+ keys with different semantics; an implementer must
know exactly which to strip and which to compare. Specification:

**Step 1 — Parse.** Parse the proposed output text using
`Code.string_to_quoted_with_comments!/2` with the same options as the
input.

**Step 2 — AST structural comparison.** Walk both trees, comparing
node tag + child list at each position. **Strip these metadata keys**
before comparison (they are positional or formatter-only and do not
affect semantics):

- `:line`, `:column`, `:end_line`, `:end_column` — source positions
- `:end_of_expression`, `:closing` — parser-internal markers
- `:format` (e.g. `:keyword` vs `:bin_string`), `:merge` — formatter
  presentation hints with no AST identity
- `:token` — token-level metadata

**Preserve and compare** these metadata keys (semantically meaningful):

- `:context` — distinguishes module-body context from top-level; a
  drift here means a `def` migrated from inside a `defmodule` block
  to outside (or vice versa) and the code no longer compiles.
- `:delimiter` — distinguishes `"..."` from `'...'` (charlist) and
  heredoc strings; a drift changes literal values.
- `:do` / `:do_end` — distinguishes `do ... end` from keyword-list
  `do: ...`; meaningful for macros that match shape.

Implementers MUST validate this strip-set against the source corpus
(at minimum the entire `erlang-test/` codebase) and confirm zero
false positives on unedited files before a writable plan kind ships.

**Step 3 — Comment sidecar comparison.** Each comment in the
`Code.string_to_quoted_with_comments` sidecar is a `%{line: int,
column: int, text: string, previous_eol_count: int,
next_eol_count: int}` map. Compare:

- **By anchor, not just count + body.** Map each comment in the input
  to a structural anchor: the next following top-level AST node
  whose start_line is ≥ comment_line + 1 (forward-attachment, matches
  `mix format`'s convention). Tie-breaks:
  - **Multiple comments before one node:** anchored to the same node,
    ordered by original comment_line.
  - **Same-line trailing comment** (`def foo, do: bar # comment`):
    anchored to the AST node ending immediately before the `#` on
    that line, with `position: "trailing"`.
  - **File-trailing comments** (no following node): anchored to a
    synthetic `__file_trailing__` token.
  Map each comment in the output the same way. **Both anchor identity
  AND comment body must match.** A comment whose body survives but
  whose anchor changed has been silently moved to the wrong clause —
  refuse with `error.roundtrip_comment_relocated`.
- `previous_eol_count` and `next_eol_count` are compared loosely (off
  by ≤1 is acceptable; the formatter normalizes blank lines).

**Step 4 — Declared comment deletions cross-validation.** Plans that
legitimately remove a comment (e.g., a comment attached to a deleted
def) must declare each removal in `expected_comment_deletions` as
`{anchor_kind: "def" | "defp" | ..., anchor_name: string,
anchor_arity: int?, comment_line: int, comment_body_prefix: string}`.
The round-trip check verifies that each declared deletion (a) appeared
in the input under the declared anchor and (b) is absent from the
output. A declared deletion that doesn't match an actual input comment
refuses with `error.spurious_comment_deletion_declared`; an
undeclared deletion refuses with `error.roundtrip_unstable`.

**Step 5 — Refusal.** If any of steps 2–4 fail, refuse with
`error.roundtrip_unstable` (or the specific code above) and the
diff; the plan does NOT apply.

**Shared implementation.** EX-V6 is implemented once as
`verify_elixir_ast_roundtrip` inside the daemon's apply machinery,
invoked automatically for every writable plan kind via the apply
gate. Individual plan kinds do NOT reimplement the check — this
prevents the "first plan kind to forget it ships without the gate"
failure mode flagged in round 2 (deepseek M-R2-8).

This rule blocks the entire class of "formatter drift drops a
comment" or "literal-encoder reshape changes evaluation semantics"
silent breakage. No Rust/Java parallel: Rust's tree-sitter substrate
emits TextEdits at byte level and bypasses AST→text round-trip;
Java's JavaParser preserves trivia by default. Elixir's
parser-serializer round-trip is lossy by default and demands this
explicit invariant.

## Atomic agent catalog (initial)

Eight atoms, parallel to the Rust batch1 catalog. Each is a JSON manifest
under `system-defaults/atoms/refactor/` following the
`../refactor-agents.md` contract. Per-atom catalogs list only the
distinguishing fields; the shared manifest contract applies.

### `elixir-shatter-dispatch-table`

Decompose a multi-clause atom-tag-dispatch function into per-tag submodules.

- **When:** A `def`/`defp` has 30+ clauses, each pattern-matching on a
  leading discriminator (atom or atom-in-struct), and operator has
  identified a partition.
- **Anti:** Do not use to *decide* a partition — chain
  `elixir-module-dependency-graph` or operator review first. Do not use
  when source clauses share heavy mutable state (no such thing in Elixir,
  but shared `@module_attr` references count). Do not use when
  duplicate-tag clauses require subkey discrimination unless operator
  supplies `duplicate_tag_policy: "explicit_subkeys"` with subkey paths.
- **Inputs:** `source_file`, `function_name`, `arity`, `head_matcher`,
  `partition`, `selection_mode: "exhaustive" | "selected_only"`,
  `duplicate_tag_policy: "group_to_same_bucket" | "explicit_subkeys"`,
  `target_dir`, `acknowledge_quote_in_moved`, `acknowledge_use_at_scope`,
  `acknowledge_defmacro_move`, `acknowledge_unpreservable_guards`,
  `acknowledge_dynamic_dispatch`, `apply`.
- **Output extension:** `partitions: [{target_module, target_file,
  clause_count, captured_helpers, shared_helpers,
  dynamic_dispatch_unresolved, duplicate_tag_groups, guarded_clauses}]`,
  `unenumerated_tags`, `roundtrip_check: {passed: bool, diff?: string}`.
- **Composition:** `chainable_after:
  ["elixir-module-dependency-graph", "elixir-public-api-guard"]`,
  `parallel_safe: false`.

### `elixir-split-genserver`

Carve a GenServer into per-concern child GenServers, retaining a parent
facade for backward compatibility. Supports both `single_dispatch_fn`
and `per_message_handle_call` source shapes.

- **When:** A GenServer module exceeds 1500 LOC or has 30+ message
  handlers and a clean concern partition exists. Both single-dispatch
  and direct-handle_call GenServers are supported.
- **Anti:** Do not use when callbacks share write access to overlapping
  state fields without `acknowledge_shared_state`. Do not use on
  GenServers that hot-code-reload critical singletons. Do not pass
  `client_api_strategy: "delegate"` with
  `dispatch_pattern: "per_message_handle_call"` — refused upfront.
- **Inputs:** `source_file`, `genserver_module_name`,
  `dispatch_pattern: "single_dispatch_fn" | "per_message_handle_call" | "mixed"`,
  `partition: {<child_module>: [<call_name_list>]}`, `apply`,
  `client_api_strategy: "delegate" | "rewrite_callers"`,
  `supervisor_module`, `acknowledge_shared_state`,
  `acknowledge_use_at_scope` (typically true for any GenServer because
  `use GenServer` is at module scope by definition).
- **Output extension:** `children: [{module, target_file, moved_messages,
  state_fields_isolated, supervisor_wiring_emitted, dispatch_pattern_resolved}]`,
  `roundtrip_check`.
- **Composition:** `chainable_after: ["elixir-genserver-state-audit"]`,
  `parallel_safe: false`.

### `elixir-facade-wire`

Regenerate `defdelegate` blocks on a facade module to mirror the backing
modules' public surfaces.

- **When:** A facade module (e.g., `Substrate`) re-exports many backing
  modules and one or more backings have gained/lost public functions
  since the facade was last regenerated.
- **Anti:** Do not use to invent a new facade — the facade module must
  exist with at least a `@moduledoc`. Do not use when backing modules
  emit functions through `defmacro` (the facade can't `defdelegate` to
  macros cleanly).
- **Inputs:** `facade_file`, `backing_modules`, `name_filter`,
  `arity_filter`, `keep_existing: bool`, `apply`.
- **Output extension:** `added`, `removed`, `kept_existing`, `renames`.

### `elixir-extract-behaviour`

Lift a function set on a module into a `@behaviour` with `@callback`
declarations.

- **When:** A module's public functions form a natural protocol and the
  operator wants mockability or multiple-implementation dispatch.
- **Anti:** Functions take anonymous fn parameters unless
  `acknowledge_anonymous_fn_callbacks: true`. Functions are
  `defmacro`s (behaviours don't dispatch macros).
- **Inputs:** `source_file`, `behaviour_module_name`, `target_file`,
  `item_names`, `generate_default_impl: bool`,
  `acknowledge_anonymous_fn_callbacks: bool`, `apply`.
- **Output extension:** `callback_signatures`, `default_impl_module`,
  `callsite_warnings`, `mfa_capture_warnings`.

### `elixir-organize-aliases`

Sort, dedupe, and collapse module-level `alias` / `import` / `require`
directives.

- **When:** Operator wants a hygiene pass before or after a structural
  refactor; alias drift accumulates after `extract_elixir_module`.
- **Anti:** Do not use on modules with `use Foo` whose macros emit
  conditional aliases.
- **Inputs:** `source_file`, `apply`.
- **Output extension:** `directives_sorted`, `directives_merged`,
  `directives_dropped`.
- **Cost class:** `cheap`.

### `elixir-public-api-guard`

Front-end for `elixir_public_api_guard`. Precondition atom chained before
any pattern that touches public surfaces.

- **When:** About to run a refactor that may modify public functions
  (extraction, behaviour adoption, GenServer split).
- **Anti:** Do not use as a permission grant; this atom reports delta,
  doesn't decide whether the change is safe.
- **Inputs:** `source`, `facade_modules`, `proposed_changes`.
- **Output extension:** `public_items_touched`, `public_api_delta_summary`,
  `facade_re_exports_affected`, `advisory_severity`.
- **Cost class:** `normal` (directory scans can be large).

### `elixir-module-dependency-graph`

Front-end for `elixir_module_dependency_analysis`.

- **When:** Before deciding a split partition; before adopting behaviour
  contracts when implementations are scattered.
- **Anti:** Do not use as a clustering algorithm — this atom returns the
  graph, not partitions.
- **Inputs:** `source_dir_or_file`, `module_filter`.
- **Output extension:** full graph from `elixir_module_dependency_analysis`.
- **Cost class:** `cheap`.

### `elixir-genserver-state-audit`

Front-end for `elixir_genserver_state_audit`. Precondition for
`elixir-split-genserver`.

- **When:** About to run `elixir-split-genserver` and want to inspect
  state-field reach across callbacks first.
- **Anti:** Do not use as a permission grant; analysis-only.
- **Inputs:** `source_file`, `genserver_module_name`.
- **Output extension:** full state-audit report.
- **Cost class:** `cheap`.

## Worked examples

Concrete plan compositions grounded in the inventory above.

### Carve `op_runtime.ex` into per-domain submodules

The actual `op_runtime.ex` is 5,175 LOC with 158 `def run/2` clauses
(verified `grep -cE '^\s*def\s+run\b'`), including multi-line clause
heads at `op_runtime.ex:570` and `:1937` (each opens `def run(data,
%Op{` across multiple lines) and same-primary-tag duplicates
(`:author_edge` at 570/590, `:emit_phase_evidence` at 1674/1680).
Carving all 158 in one pass is unrealistic — the workflow is
incremental under `selection_mode: "selected_only"`:

```
elixir-module-dependency-graph(source="apps/substrate/lib/substrate/op_runtime.ex")
→ operator review of clause-helper graph
elixir-public-api-guard(source="apps/substrate/lib/substrate/op_runtime.ex",
                        facade_modules=["Substrate"])
→ delta clean (`run/2` stays on parent; callers unaffected)
elixir-shatter-dispatch-table(
    source="apps/substrate/lib/substrate/op_runtime.ex",
    function_name="run",
    arity=2,
    head_matcher={
      "discriminators": [
        {"arg_index": 1, "binding": "%Op{kind: $TAG}", "primary": true},
        {"arg_index": 1, "binding": "%Op{kind: $TAG, args: $ARGS}", "secondary": true}
      ],
      "preserve_guards": "verbatim"
    },
    selection_mode="selected_only",                # pull a subset; leave rest on router
    duplicate_tag_policy="group_to_same_bucket",   # :author_edge clauses stay together
    partition={
      "Substrate.OpRuntime.Entity":  [":resolve_entity", ":invoke_projection",
                                      ":format_object"],
      "Substrate.OpRuntime.Explain": [":explain_tool_exposure",
                                      ":explain_workflow_ratification",
                                      ":explain_impact_set"]
    },
    target_dir="apps/substrate/lib/substrate/op_runtime",
    acknowledge_use_at_scope=true,                 # op_runtime imports Op struct from a use'd module
    apply=true)
→ bbox_refactor_run with
    [<the plan>,
     {"op":"command","command":"mix","args":["compile","--warnings-as-errors","--return-errors"],
      "capture":"mix_diag","on_failure":"continue_for_repair"},
     {"op":"plan","kind":"elixir_compile_fix_round","diagnostics_ref":"last"},
     {"op":"command","command":"mix","args":["compile","--warnings-as-errors"],"required":true},
     {"op":"command","command":"mix","args":["format","--check-formatted"],"required":true},
     {"op":"command","command":"mix","args":["test","--max-failures","1"],"required":true}]
```

Expected outcome (first pass): only the Entity and Explain clauses move;
remaining ~150 clauses stay on the router untouched. Round-trip check
verifies comment preservation across the moved clauses. Subsequent
passes carve additional buckets via the same atom invocation pattern
with different partition entries. The long tail of singleton tags
(`:promote_tool`, `:apply_suspend_tool`, etc.) gets handled when an
operator either (a) decides the singleton belongs in an existing bucket
or (b) accepts it staying on the router permanently — neither is a
refactor failure mode.

Same workflow applies for `defp` splitting (e.g.,
`verifier.ex`'s ~30 `defp apply_invariant(:tag, ...)` clauses). The
`external_callers` report is skipped for `defp` because callers are
necessarily in-module.

### Split `admin_endpoint.ex` by concern

```
elixir-genserver-state-audit(source="apps/substrate/lib/substrate/admin_endpoint.ex",
                             genserver_module_name="Substrate.AdminEndpoint")
→ state fields are %{pending, refs}; checkpoint-admin reads/writes only
  the pending/refs pair; entity-getters read nothing (pure delegate).
→ dispatch_pattern detected as "single_dispatch_fn" (one generic handle_call
  at admin_endpoint.ex:145 delegating to defp dispatch/1 at :198).
elixir-split-genserver(
    source="apps/substrate/lib/substrate/admin_endpoint.ex",
    genserver_module_name="Substrate.AdminEndpoint",
    dispatch_pattern="single_dispatch_fn",       # required input now
    partition={
      "Substrate.AdminEndpoint.Checkpoint":   ["verify_checkpoint", "canonical_hash",
                                               "journal_range", "pin_known_good",
                                               "rollback_to", "force_restart"],
      "Substrate.AdminEndpoint.Schema":       ["all_entity_type_defs", "all_edge_type_defs",
                                               "all_index_configs", "all_effect_policies",
                                               "all_workflows", "all_packets",
                                               "all_graph_rules"],
      "Substrate.AdminEndpoint.Entities":     ["all_decisions", "all_evidence",
                                               "all_concepts", "all_correspondences",
                                               (...)]
    },
    target_dir="apps/substrate/lib/substrate/admin_endpoint",
    client_api_strategy="delegate",
    supervisor_module="Substrate.Application",
    acknowledge_use_at_scope=true,                # admin_endpoint has `use GenServer`
    support_callbacks=[                           # admin_endpoint.ex:156, :169
      {"callback": "handle_info/2",
       "pattern_match": "{ref, reply}",
       "disposition": "move_with_async_group"},
      {"callback": "handle_info/2",
       "pattern_match": "{:DOWN, ref, :process, _pid, reason}",
       "disposition": "move_with_async_group"}
    ],
    apply=true)
→ bbox_refactor_run with the same compile+fix+format+test sequence as above
```

Expected outcome: parent `Substrate.AdminEndpoint` becomes a thin facade
(client API delegating to children); each concern is its own GenServer
under `Substrate.AdminTaskSupervisor`; async-task handling stays with
`Substrate.AdminEndpoint.Checkpoint` where it's actually used. For a
hypothetical traditional-shape GenServer with one `handle_call` per
message, the same atom invocation works with
`dispatch_pattern="per_message_handle_call"` and `client_api_strategy`
constrained to `"rewrite_callers"`.

### Regenerate the `Substrate` facade

```
elixir-facade-wire(
    facade_file="apps/substrate/lib/substrate.ex",
    backing_modules=["Substrate.Graph", "Substrate.ProcessScope",
                     "Substrate.OutwardObservation.RuntimeState",
                     ...],
    name_filter="^(put_|get_|all_|delete_).*",
    keep_existing=true,
    apply=true)
```

Expected outcome: any new entity types added to backing modules since
the last regenerate get `defdelegate` lines added; removed functions get
their delegations dropped; existing custom delegations preserved.

## Open Design Questions

1. **AST helper deployment — favored option is a daemon-managed escript.**
   The daemon needs an Elixir process to produce the authoritative
   quoted form and run `Code.with_diagnostics/2` captures.
   Options evaluated:
   - **(a) `mix run` per call:** ~1–2s mix boot per refactor plan. A
     5-step compound run incurs 5–10s of mix boot overhead — unacceptable
     for interactive workflows. **Rejected.**
   - **(b) Embedded OTP via Rust ↔ BEAM interop:** lowest latency but
     adds substantial operational complexity (matching OTP version per
     project, managing node names, crash recovery). Not v1.
   - **(c) Tree-sitter-only:** insufficient — tree-sitter doesn't
     resolve aliases or expand macros, both needed for the writable
     lane round-trip and for accurate dependency analysis.
   - **(d) ★ Daemon-managed escript with project-root pinning:** compile
     a long-running `priv/elixir_ast_helper` via `mix escript.build`,
     which produces a self-executing archive that bundles its own
     `elixir` plus all dependencies. The daemon launches one helper per
     registered project root, communicates over stdin/stdout JSON
     framing, and recycles the helper on project version change. Cold
     start is ~1.5s but only once per project; per-call latency is
     ~10ms. This is the favored v1 path.

   The escript path has one wart: escript embeds the Elixir version it
   was compiled against. To match the project's Elixir version, the
   helper is rebuilt by the daemon when it detects a project version
   skew. The build itself uses `mix escript.build` inside the project
   root, so the helper picks up the project's Elixir tooling. Open
   sub-question: should rebuilds happen lazily (on first version-skew
   detection) or eagerly (at project registration)? Default: lazy.

2. **Lexical vs elixir-ls.** elixir-ls is more mature; lexical is faster
   and the modern path. v1 ships with elixir-ls; lexical support is a
   provider swap at the LSP-session layer. Open whether to ship both
   bindings or wait for community settling. **Rename support is
   unavailable on both as of 2026-05** (see EX-G5 matrix), so the LSP
   choice mainly affects pipe-chain/macro-expand backends.

3. **`mix xref` data freshness.** `elixir_module_dependency_analysis`
   prefers in-process xref computation via the AST helper (option c in
   the round-1 doc), avoiding `_build/` mutation. The helper imports
   `Mix.Tasks.Xref` and runs it against an in-memory project graph.
   Open: whether the helper should accept a `--refresh-build` flag for
   the (rare) cases where the in-memory graph diverges from a real
   compile.

4. **Codegen snapshot directory policy.** `elixir_codegen_audit` writes
   snapshots under `priv/codegen_snapshots/` by default. Whether these
   should be committed (as regression artifacts) or gitignored (as
   reviewable transient outputs) is a per-project decision. The atom
   ships them as committed by default; operators can override.

5. **`elixir_dialyzer_attribution` PLT management.** Dialyzer warmup is
   slow. The plan kind assumes a warm PLT in `_build/dev/dialyxir/`.
   Cold-start handling: (a) refuse with `error.dialyzer_plt_cold` and
   require operator to run `mix dialyzer --plt`; (b) warm in-band (slow
   first run); (c) compose a `mix dialyzer --plt` step inside
   `bbox_refactor_run`. Option (a) is the honest default.

6. **Behaviour adoption and typespec coverage.** `extract_elixir_behaviour`
   needs `@spec` per lifted function to generate `@callback` decls. When
   source has no `@spec`s, the planner emits `@callback foo(any()) ::
   any()`. Open whether this is acceptable (callback contract is
   trivially satisfied) or should refuse and require operator to add
   `@spec`s first.

7. **`captured_attributes` handling strategy.** `split_elixir_clauses_by_tag`
   reports `@module_attr` references used by moved clauses but does
   not specify how the planner reconstructs the attribute on the
   target. Options: (a) copy the attribute definition verbatim to the
   target, refusing if the definition depends on other unmoved
   attributes; (b) evaluate the attribute at plan time and inline the
   value into the target; (c) report-only — emit FIXMEs on moved
   clauses and let the operator decide. Default v1: option (a) with
   refusal on cross-attribute dependencies; downgrade to option (c)
   when the attribute value contains
   `Application.compile_env`/`get_env` (compile-time vs plan-time
   evaluation asymmetry).

8. **`acknowledge_use_at_scope` verification.** The flag bypasses the
   pre-condition refusal but does NOT verify that the move actually
   preserved the post-`use`-expansion public surface. A v1 add-on
   path: after apply, compile both source and target, capture the
   post-expansion public function set via `Module.definitions_in/2`,
   diff. Refuse rollback on equivalence; commit on match. v1 ships
   without this verification (the operator's acknowledgment is the
   contract); v2 adds the compile-and-verify pass.

## Closed Design Questions

The following questions from the round-1 draft are now resolved:

- **AST substrate writable vs analysis lane** — resolved in Substrate
  decisions: writable lane uses `string_to_quoted_with_comments!/2`
  with EX-V6 round-trip preservation; analysis lane uses
  `string_to_quoted!/2`. `:literal_encoder` was dropped (the Elixir
  docs warn its output is not valid for normal evaluation).
- **`acknowledge_macro_expansion` granularity** — resolved by splitting
  into `acknowledge_quote_in_moved`, `acknowledge_use_at_scope`,
  `acknowledge_defmacro_move` in EX-V1.
- **Compile-fix-round JSON delivery** — resolved by using
  `Code.with_diagnostics/2` in the AST helper; no `mix.exs` mutation.
- **`mix format --check-formatted` in EX-V2** — resolved by classifying
  it as read-only (no `touches` required).

## Deferred (with criteria)

Round-1 hit: some round-1 "rejected" items had thinner evidence than
the rejection implied. Reclassified as deferred with explicit
acceptance criteria for promotion:

- **`elixir_atom_dispatch_to_protocol`** — convert multi-clause
  function-head dispatch into a `defprotocol` + per-impl modules.
  Deferred, not rejected. Promotion criteria: (1) the dispatch is on
  a data-type discriminator (not a free-form atom tag), (2) the call
  site is on a non-hot path (microbenchmark required to validate), AND
  (3) operator passes `acknowledge_public_api_change: true` because
  `defprotocol` introduces a new module surface. `split_elixir_clauses_by_tag`
  remains the right move for the op_runtime.ex / verifier.ex shape;
  protocol promotion applies to a different (rarer) case.

- **GenServer → `gen_statem` migration** — promote a GenServer with
  enough state-machine shape (explicit state field, transition logic in
  `handle_call`) to `:gen_statem` or `GenStateMachine`. Promotion
  criteria: (1) state field is an atom or small enum, (2) >70% of
  handle_call clauses guard on the state field, (3) operator-supplied
  transition table. Out of scope for v1 — high analysis cost, narrow
  applicability.

- **`Application.get_env` extraction** — move `Application.get_env(:app,
  :key)` calls out of hot callback paths into `init/1` or `start_link/1`.
  Promotion criteria: (1) the callback is one of `handle_call`,
  `handle_cast`, `handle_info`, (2) the key is a compile-time literal
  atom, (3) the get_env result type is stable (operator confirms).
  Currently a hand-edit; mechanization plausible but low priority.

## Rejected

- **`elixir_inline_anonymous_to_mfa`** — converting `fn x -> Foo.bar(x,
  ctx) end` callbacks to `{Foo, :bar, [ctx]}` MFA tuples. Style choice,
  not a refactor primitive; cost/benefit doesn't justify mechanization.

- **`elixir_lombokify`** — generating boilerplate accessors. Elixir's
  struct + bang/non-bang convention doesn't have the OO accessor
  problem Java does; there's nothing to mechanize.

- **`elixir_migrate_edition`** — Elixir has no edition concept; the
  closest is OTP version migration (`mix.exs` `:elixir` requirement
  bumps) which is a toolchain concern, not a refactor primitive.

- **`elixir_expand_macro`** — proc-macro expansion requires compilation;
  this would be a `mix run` shell-out or `Code.eval_string` call, not
  a refactor plan kind. The audit pattern (`elixir_codegen_audit`)
  captures the legitimate use case (regression snapshots for review).

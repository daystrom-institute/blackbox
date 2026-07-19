---
title: "Rust Isolate Surface - the rust.* cell bindings"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - rust
tags:
  - refactor-tools
  - rust
  - bro-harness
  - lsp
  - gap-notes
date: 2026-07-18
status: "design proposal, pre-implementation"
brief: "Curated rust.* / analysis.* / lsp.* binding surface for the bro-harness isolate, ported from the retiring v1 bbox_refactor rust catalog and extended with rust-analyzer's assist pipeline; the compile-fix loop (build.gate rustc diagnostics + rust.fixRound) is the centerpiece that makes the tier ladder tree-sitter -> rust-analyzer -> rustc total."
---

# Rust Isolate Surface

The daemon refactor MCP surface is retired (worktree `retire-refactor-mcp`,
commit `99413f7d`: 29 tools, `DaemonRefactor`, `bbox-macros`, `bbox-code-nav`,
and the daemon warm LSP pool removed; atoms/workflows/`sm-refactor-*` re-point
is the tracked follow-up). Refactor tooling is harness-native (decision
`af3c4783`, `b8dc263d`; [refactor-tools-v2](../../bro-harness/refactor-tools-v2.md)
§6-§7). The isolate ships `java.*` at transform altitude (33 tools), but the
rust side is anemic: `code.*` facts, the rust arm of `code.signature`, `lsp.*`
verbs wired to rust-analyzer, and the `edits.*` algebra. Every structural rust
move today is cell-authored bytes floored at `syntax_only`, while the richest
rust machinery in the building (the landed RX expansion: capture/borrow
classification, the compile-fix round, crate extraction) sits unprojected in
`crates/bbox-refactor`.

This doc designs the rust cell surface: what becomes a binding, what becomes a
recipe, what gets dropped, and the two semantic upgrades (`lsp.assist`,
SSR-via-`lsp.executeCommand`) that make the surface better than the v1 catalog
it replaces, not merely equal to it.

**Scope decision (operator, 2026-07-18):** the v2 §7 "used-kind parity" mining
pass is waived. This machine historically has little rust MCP activity to
mine, and it is becoming the primary dev box. Curation here is a priori,
validated by dogfood probes (§10) instead of transcript mining.

## 1. Current state

| Namespace | Rust status today |
|---|---|
| `code.*` (tree-sitter facts) | Works: items/query/read/readLines/spanUnion; `code.signature` has a Rust extractor. `code.fields` is Java-only. |
| `edits.*` (edit algebra, sole writer) | Fully rust-capable; post-apply parse validation uses the rust grammar. |
| `lsp.*` (language-server authority) | rust-analyzer wired: `rename`, `hover`, `willRenameFiles`, `executeCommand`, `status`. Fails closed (RX-V3). |
| `analysis.*` (Rust-side reductions) | Java-only. Every description says "Java class/file". |
| `java.*` (transforms) | 33 tools; the v1-planner-as-binding pattern this doc extends to rust. |
| `build.gate` | Parses javac/Gradle diagnostics; cargo output parses as `generic` (unusable for repair loops). |

The v1 rust catalog being mined for ports lives in `crates/bbox-refactor/src/`
(~20 `rust_*.rs` modules plus dispatch in `lib.rs`): syntactic moves, caller
rewrites, module wiring, visibility ceremonies, LSP-backed kinds, analysis
kinds, the compile-fix round, and four run-only compound expansions.

## 2. Design spine

### 2.1 Curate, do not port 1:1

The v2 diagnosis of v1: "a programming language that predates having one":
parameters that are really function calls, composites pretending to be kinds,
`bbox_refactor_run` as a hand-rolled JSON workflow interpreter. A 35-kind
`rust.*` namespace rebuilds that mistake inside the cell DSL. Decision rule,
consistent with how `java.*` was cleaved:

- A v1 kind becomes a **binding** iff it embodies host-side analysis or
  synthesis a cell cannot reasonably redo: dependency closures, capture and
  borrow classification, cross-file caller rewrites, visibility ceremonies,
  crate scaffolding, diagnostic classification.
- A v1 kind that was **orchestration** (the run-macro expansions:
  `split_rust_impl_methods_to_submodule`, `migrate_rust_mods_to_lib`,
  `extract_rust_crate`) becomes a **recipe**: a checked-in cell program.
  These were sequences of primitives plus cargo gates; sequencing is what
  cells are for.
- Micro-primitives collapse. `add_rust_mod_decl`, `add_rust_use_decl`,
  `copy_rust_mod_decls`, `rewrite_rust_mod_visibility` fold into one wiring
  transform; the two remaining visibility kinds fold into one
  `rust.setVisibility`.
- Deletion needs no binding. `delete_rust_items` existed to wrap refusals
  around the plan envelope; in v2, `code.items` mints the span and
  `edits.delete` + parse validation + findings is the safety. Gone.

Target altitude: ~15 `rust.*` transforms plus a `rust.describe` contract
tool, matching the `java.*` precedent (compact one-line-per-tool namespace
index, full contracts at runtime).

### 2.2 The rust semantic ladder is tree-sitter -> rust-analyzer -> rustc

The three-tier taxonomy (`syntax_only` / `indexed_hints` / `lsp_verified`)
already exists. Rust adds a fourth oracle Java could not lean on as hard: the
compiler itself is a cheap, total authority (`cargo check` is fast; the Java
side's Gradle builds were not). This makes the **compile-fix loop the
centerpiece of the rust surface**, not an accessory:

```
rust.extractImplMethods -> edits.apply -> build.gate("cargo check --message-format=json")
  -> rust.fixRound(diagnostics) -> edits.merge -> edits.apply -> (fixed point, cap ~5)
```

Two substrate gaps block this today:

- `build.gate` does not parse rustc JSON into bounded, span-anchored
  diagnostics the way it parses javac/Gradle.
- `rust.fixRound` does not exist as a binding. The v1
  `rust_compile_fix_round` classifier is portable verbatim: it buckets rustc
  diagnostics into add-use / visibility-bump / machine-applicable replace
  proposals, with borrow-checker and trait-bound errors as explicit
  `leftovers`. Clippy rides free (same JSON format), which closes gap G13
  (`rust_clippy_fix_round`) as a lint-classification mode of the same tool.

**Provenance (decided, §8.1):** rustc `MachineApplicable` suggestions are
compiler-authored: stronger than cell bytes, not server-signed. A
`compiler_suggested` ledger tier sits between `syntax_only` and
`lsp_verified`, recorded per-change and only for edits whose span and
replacement come verbatim from a rustc/clippy `suggested_replacement`
(producer `rust.fixRound`). Classifier-synthesized proposals (add-use,
visibility-bump) floor at `syntax_only`; they are planner guesses informed
by compiler output, not compiler bytes. Tiers are authorship lineage, not
outcome guarantees; the terminal `cargo check` in every recipe is the
outcome gate.

The multi-round discipline comes from the RX-C1 v2 resolution: diagnostics-
settling fixed point, hard cap ~5 rounds, leftovers surfaced rather than
retried.

### 2.3 The biggest semantic lever is exposing rust-analyzer's assist pipeline

`bro-lsp` (single 73K `lib.rs`) speaks no `codeAction` today. Adding
`codeAction` + `codeAction/resolve` and one `lsp.assist({span, kind?, title?})`
binding converts RA's assist catalog (extract-function, extract-variable,
inline, add-missing-impl-members, generate-derive, ...) into `lsp_verified`
transforms through the same WorkspaceEdit -> hash-anchored-changes pipeline
`lsp.rename` already uses. One binding, and three gap-inventory entries get
their correct answer:

- G11 (`rust_ra_extract_function`): RA's extract-function handles borrows and
  control flow correctly, the exact thing v1's `extract_rust_function_region`
  could only refuse. **Do not port the v1 region extractor.**
- G20 (`rust_generate_derives`): the generate-derive assist.
- G16-class signature surgery partially covered; see the coverage matrix (§6)
  for the known soft spot.

Two more upgrades ride the same plumbing:

- `lsp.references` (RA `textDocument/references`; JDTLS symmetric): the
  authoritative lane for G10 find-references. Java parity argument: JDTLS
  backs `find_java_usages`. Complements the fast syntactic count lane in
  `analysis.references`.
- `lsp.executeCommand` WorkspaceEdit conversion: when a command result is a
  WorkspaceEdit, hash-anchor it host-side exactly like `lsp.rename` output
  instead of returning raw JSON. This unlocks `rust-analyzer.ssr` (structural
  search-replace, binding-aware find/replace with placeholders) as an
  `lsp_verified` campaign tool with zero new server code. The post-shipment
  gap map records the largest Java campaign running on hand-rolled JS
  char-index byte math; SSR is the rust answer to that class of pain.

A small `lsp.definition` verb (goto-declaration at a span) falls out of the
same plumbing and subsumes v1's `rust_ra_classify_callbacks` use case.

### 2.4 RX-V1 flags arrive dispatch-side via `ToolArgDefaults` lookup

The trust model: operator authority arrives dispatch-side, never as a cell
argument (a cell-authored `acknowledge_*` is confirm theater).
`rust.moveStructFields` (`acknowledge_repr`), `rust.migrateErrorType`
(`acknowledge_public_api_change`), and `rust.migrateTypeUsages`
(`acknowledge_public_api_change`) consume RX-V1 opt-outs. No `java.*`
transform needed this channel yet, so it is ratified here (decided, §8.2):

- The binding declares **no** `acknowledge_*` param in its schema. A cell
  passing one gets an error naming the channel ("operator authority arrives
  dispatch-side; set via dispatch config").
- The binding queries `cx.tool_arg_defaults` host-side for a rule on
  `(tool, param)`. This needs a small public `lookup(tool, param)` accessor
  on `ToolArgDefaults` (today only `apply` and `validation_warnings` are
  public; the pattern matching already exists).
- Rule present and true: proceed, and record `operator_opt_outs_used` on the
  applied EditSet's lineage metadata (the v1 audit field, preserved on the
  v2 artifact). Absent: refuse with the RX-V1 refusal.

The naive alternative (letting the seam's input-merge deliver the flag) is
rejected: merged input is indistinguishable from a cell-authored flag, and
the `ToolArgRider` that would expose the difference attaches to the result,
after the decision. The lookup path makes the cell-authoring case a schema
error and the operator-granted case mechanically auditable. The isolate CLI
grows a defaults flag (the `ToolArgDefaults::parse_map` entry point exists)
so probes can exercise the granted path.

### 2.5 Kill the second LSP pool while porting

Pressure-test delta D4: `bbox-refactor` drags `bbox-lsp` + rmcp into the
harness, yielding two warm LSP pools. The LSP-backed v1 rust kinds
(`rust_lsp_rename`, `rust_organize_imports`, `rust_ra_move_item_to_module`,
`rust_ra_classify_callbacks`) are the only crate consumers of `bbox-lsp` on
the rust side. As their `lsp.*` / `rust.*` replacements land, delete those
kinds from `bbox-refactor` so the crate drops the `bbox-lsp` dependency. All
LSP access in bindings goes through the session `LspState` (the
`lsp_facts.rs` pattern, which `java_transforms` already follows). Never
replicate `rust_ra_move_item`'s private `LspSessionManager::new()`
construction; that was an anti-pattern even in v1.

## 3. The rust surface

Naming follows the `java.*` camelCase verb style. Port sources are v1
`bbox-refactor` kinds; "port" means the documented recipe in
`crates/bro-harness/src/bindings/AGENTS.md`: run the v1 analysis/synthesis
verbatim, strip the MCP/plan-apply envelope, return `{changes, creates,
findings}`, never write.

### 3.1 `rust.*` transforms

Move and extract:

| Binding | Port source | Notes |
|---|---|---|
| `rust.extractItems` | `extract_rust_items_to_submodule` + `extract_rust_items` + `move_rust_items_with_local_deps` + `extract_rust_section` | One transform: plain extract when wiring knobs unset; compound mode does scaffold + `mod` decl + visibility bumps (items and struct fields) + auto-pruned `use` decl. `with_local_deps` knob moves the private dependency closure. `section` knob takes marker/line bounds. Knobs select the shape of synthesis with the same host analysis running either way (the resolved boundary test, §8.3); analysis is always on and reports in findings, never knob-gated. |
| `rust.extractImplMethods` | `extract_rust_impl_methods` | God-impl workhorse: `super::` rebasing, visibility widening on parent residue, attribute preservation. rmcp `tool_router` wrapper generation stays out (repo-specific; recipe material). |
| `rust.extractTrait` | `extract_rust_trait` | Trait extraction with object-safety findings, `trait_in_scope_required` report. |
| `rust.inlineModToFile` | `inline_mod_to_file_submodule` | Inline `mod foo { ... }` to sibling file; outer attrs (`#[cfg(test)]`) stay attached. |
| `rust.rewriteModuleCallers` | `move_rust_items_with_callers` (decomposed) | Caller-prefix rewrite only, composable after any extract or move. v1 fused extract+rewrite for hash consistency; v2 sequences two applies, each self-validating. |
| `rust.moveStructFields` | `move_rust_struct_fields` (RX-S1) | `remaining_source_accessors` report; refuses non-default `#[repr]` without dispatch-side `acknowledge_repr` (§2.4). |
| `rust.updateCallers` | `update_rust_callers` (RX-S2b) | Pessimistic caller rewriter: Copy-whitelisted reads and unambiguous calls rewritten; writes/destructures/spreads/`mem::*` to `unrewriteable_accessors`. |
| `rust.addDelegateField` | `add_rust_delegate_field` (RX-S2a) | Delegate field + constructor wiring. |
| `rust.liftToFree` | `lift_rust_inherent_to_free` | Zero-`self` inherent methods to free functions; explicit lifetimes verbatim, never re-elided. |

Wiring and hygiene:

| Binding | Port source | Notes |
|---|---|---|
| `rust.moduleWiring` | `rust_module_wiring` (+ absorbed mod/use micro-kinds) | One conservative module-graph edit: add/remove `mod`, add/remove `use`, idempotent, rejects duplicates/missing. |
| `rust.setVisibility` | `rewrite_rust_item_visibility` + `rewrite_rust_field_visibility` | Item/impl-method/field selector; preserves `async`/`unsafe`/`const` qualifiers. |
| `rust.organizeImports` | `rust_organize_imports` + `rust_minimize_imports` | RA `source.organizeImports` (`lsp_verified`); `minimize` mode runs the syntactic wildcard-minimizer first (`indexed_hints`). |

Type migration:

| Binding | Port source | Notes |
|---|---|---|
| `rust.migrateTypeUsages` | `migrate_rust_type_usages` | `replacement_kind` legality per position; skips turbofish/patterns/TypeId; hints-tier honesty about shadowing. |
| `rust.migrateErrorType` | `rewrite_rust_error_type` (RX-E1) | Signature + strict construction-form mapping; `?` sites classified, never auto-converted; `acknowledge_public_api_change` dispatch-side only. |
| `rust.migrateStringFieldToEnum` | `migrate_rust_string_field_to_enum` | Phase 3; niche but the synthesis exists. |

Crate level and repair:

| Binding | Port source | Notes |
|---|---|---|
| `rust.extractCrateScaffold` | `extract_rust_crate_scaffold` | Atomic leaf-only peel: crate scaffold, file moves, `use <crate>::<m>;` alias swap, workspace wiring; fails closed listing residual `crate::<other>` references. The v1 run expansion around it becomes the crate-peel recipe (§5). |
| `rust.fixRound` | `rust_compile_fix_round` + clippy mode (G13) | Classifies rustc/clippy JSON diagnostics into mechanical edit proposals (add-use, visibility, machine-applicable replace) + explicit leftovers. The loop engine of §2.2. |
| `rust.describe` | namespace convention | Full per-transform contracts at runtime. |

### 3.2 `analysis.*` goes bilingual

Per the two-tier rule (facts in `code.*`, reductions Rust-side in
`analysis.*`; raw intermediates never enter the isolate heap, gap-fb7a1f99),
rust structural questions land in `analysis.*` with a language dimension, not
in rust-private verbs:

| Binding | Port source | Notes |
|---|---|---|
| `analysis.implPartition` | `rust_impl_partition_analysis` | Impl-method call/state graph for split planning. |
| `analysis.topLevelDeps` | `rust_top_level_dependency_analysis` | Item dependency graph + external reference hints + suggested clusters; the pre-extract survey. |
| `analysis.publicApi` | `rust_public_api_guard` | Public-surface advisory for visibility/API edits. |
| `analysis.references` (rust mode) | new (G10 fast lane) | Syntactic per-symbol counts/file lists with rust usage kinds (call, type_ref, path_ref, macro_use); the Java tool's shape, rust grammar. |
| `analysis.workspaceDag` | `rust_workspace_dag_check` | Workspace path-dependency acyclicity; phase 3, feeds crate peels. |

`analysis.describe` carries the per-language contracts and takes an optional
language filter. The namespace index line carries the language tag per verb
(`analysis.implPartition (rust): ...`), and Java-shaped verbs
(`cohesionClusters`, `fieldClassification`, ...) stay honest about being
Java-only in the index line. The divergence rule (§8.5): a verb name is
shared across languages only when its contract is structurally identical
(`references` qualifies); the moment a contract would diverge per language,
the verb gets a language-specific name.

### 3.3 `lsp.*` extensions (shared with Java)

| Binding | Server feature | Notes |
|---|---|---|
| `lsp.references` | `textDocument/references` | Authoritative find-usages (G10 authoritative lane); JDTLS symmetric. |
| `lsp.assist` | `codeAction` + `codeAction/resolve` | List-then-apply: bare call returns the server's offered actions at the span (kind/title/index, capped); `select` applies one through resolve -> WorkspaceEdit -> hash-anchored changes. Free-form kind/title filter, no allowlist (§8.4). Mechanical guards: snippet-edit actions flattened or refused, command-returning actions refused with a pointer at `lsp.executeCommand`. Unlocks extract-function (G11), generate-derive (G20), inline, add-missing-impl-members. `lsp_verified` lineage. |
| `lsp.definition` | `textDocument/definition` | Goto-declaration at a span; subsumes the `rust_ra_classify_callbacks` use case. |
| `lsp.executeCommand` (upgrade) | result conversion | WorkspaceEdit results hash-anchored host-side like `lsp.rename` output. Consults `executeCommandProvider.commands` and fails closed on unregistered commands. (SSR does NOT ride this: RA 1.96 serves `rust-analyzer.ssr` only via codeAction, so SSR folds into `lsp.assist`.) |

`bro-lsp` grows `codeAction`, `codeAction/resolve`, `references`,
`definition`; all fail closed (RX-V3), all bounded (capped result lists, no
unbounded workspace edits without the aggregate-cap discipline).

### 3.4 Generic promotions

- `toml.ensureTable` (promote v1 `ensure_toml_table` to a generic binding):
  idempotent TOML table merge; `Cargo.toml` edits inside crate-peel and
  feature-flag recipes.
- File moves are a recipe, not a binding: `lsp.willRenameFiles` (semantic
  path rewrites) + `edits` for the `mod` decl + `rust.moduleWiring`.

## 4. Deliberately dropped

| v1 kind | Disposition |
|---|---|
| `rust_ra_move_item_to_module` | Broken by RA design (its move assists accept no caller destination). Delete from `bbox-refactor`; do not port. |
| `rust_ra_classify_callbacks` | Subsumed by `lsp.definition` / `lsp.references`. |
| `extract_rust_function_region` | Superseded by `lsp.assist` extract-function (semantic, borrow-correct). Do not port the syntactic refuser. |
| `add_rust_router_to_sum` | rmcp `tool_router` wiring is repo-specific. Recipe material. |
| `delete_rust_items` | `code.items` span + `edits.delete`; no binding. |
| `move_file` | Recipe over `lsp.willRenameFiles`. |
| `rewrite_rust_bin_crate_paths` / `rewrite_rust_crate_paths` | Absorbed into `rust.extractCrateScaffold` + crate-peel recipe; simple prefix cases covered by `rust.rewriteModuleCallers`. |
| Run-macro compounds (`split_rust_impl_methods_to_submodule`, `migrate_rust_mods_to_lib`, `extract_rust_crate` expansion) | Become recipes (§5). |
| `rust_match_arm_to_strategy` | Deferred; niche. Revisit after phase 3 or keep as recipe. |
| Macro-expansion-aware anything | Rejected territory per the gap inventory. |

## 5. Recipes (orchestration dissolves into cell programs)

Checked in under the delegation prompt (§9), each a short cell program with a
terminal `cargo check` gate:

- **Monster-file split**: `analysis.topLevelDeps` -> `rust.extractItems` ->
  compile-fix loop.
- **God-impl split**: `analysis.implPartition` -> `rust.extractImplMethods`
  -> compile-fix loop. (Replaces `split_rust_impl_methods_to_submodule`.)
- **Bin-to-lib migration**: `rust.moduleWiring` (copy decls) + path-rewrite
  via `rust.rewriteModuleCallers` -> per-bin `cargo check` loop. (Replaces
  `migrate_rust_mods_to_lib`.)
- **Crate peel**: `analysis.workspaceDag` -> `rust.extractCrateScaffold` ->
  `toml.ensureTable` consumer wiring -> `cargo check --workspace` loop.
  (Replaces the `extract_rust_crate` expansion.)
- **File/module move**: `lsp.willRenameFiles` + `rust.moduleWiring`.
- **Error-type migration**: `rust.migrateErrorType` -> compile-fix loop;
  `?`-site leftovers are the manual punch list, by design.

## 6. Coverage matrix (what "complete" means)

| Campaign | Surface path | Tier reached |
|---|---|---|
| Split a monster file | `analysis.topLevelDeps` -> `rust.extractItems` -> fix loop | compiler-gated |
| God-impl split | `analysis.implPartition` -> `rust.extractImplMethods` -> fix loop | compiler-gated |
| Inline mod to file | `rust.inlineModToFile` | syntax_only + parse validation |
| Move module/file | `lsp.willRenameFiles` recipe | `lsp_verified` |
| Extract trait | `rust.extractTrait` (+ `lsp.assist` add-missing-impl-members) | mixed, compiler-gated |
| State extraction | `rust.moveStructFields` -> `rust.updateCallers` -> fix loop | `indexed_hints`, compiler-gated |
| Error-type migration | `rust.migrateErrorType` -> fix loop | compiler-gated |
| Rename | `lsp.rename` | `lsp_verified` |
| Import hygiene | `rust.organizeImports` | `lsp_verified` |
| Find usages | `analysis.references` (fast) / `lsp.references` (authoritative) | hints / `lsp_verified` |
| Extract function/variable | `lsp.assist` | `lsp_verified` |
| Structural campaign | `lsp.executeCommand` SSR -> `edits.merge` | `lsp_verified` |
| Crate peel | §5 recipe | compiler-gated |
| Signature change | manual cell edits + fix loop | known soft spot: RA has no strong change-signature assist; the compile-fix loop carries it |

## 7. Engineering notes

- **File layout**: do not write `rust_transforms.rs` as one file.
  `java_transforms.rs` is 698K and is itself the next splitting candidate.
  Start rust as a directory: `rust_transforms/{move,wiring,migrate,crates,fix}.rs`.
- **Adapter discipline**: port per the bindings AGENTS.md recipe. Watch the
  recorded footguns: planner-emitted new files arrive as `0..0` edits against
  the empty-file hash and must become `creates`; transforms are not
  idempotent over their own output (target-exists refusal is the DONE
  signal); findings must be repairable without re-running discovery.
- **Two LSP pools**: §2.5. `bbox-refactor` sheds its `bbox-lsp` dependency as
  the LSP-backed v1 rust kinds are deleted; bindings take session `LspState`.
- **Isolate heap discipline**: any fan-out binding (`analysis.references`
  rust mode, `lsp.references`) bounds its payload and reports caps, per the
  `code.*` aggregate-cap precedent.
- **RX-V1 audit**: `operator_opt_outs_used` recorded on the applied EditSet
  lineage, granted dispatch-side via the §2.4 lookup channel.
- **Validation**: every binding gets an `isolate --describe` contract check
  and `--cell` probes against fixtures, per the PROJECT.md isolate runbook;
  friction files gaps in the `*/refactor-tools/*` dedupe namespace via the
  RETRO loop (`prompts/RETRO_ISOLATE_REFACTOR.md`). Rust test isolation
  invariants (tempdir canonicalization, real-HOME isolation) apply to the
  binding tests; nextest `--workspace` is the gate.

## 8. Design decisions (resolved 2026-07-18)

The five open questions from the draft, resolved with the operator. Each
entry records the decision, the reasoning, and the rejected alternative.

### 8.1 Provenance: `compiler_suggested` tier, per-change, verbatim bytes only

Decision: add a `compiler_suggested` variant to the ledger's `AuthorityTier`
between `SyntaxOnly` and `LspVerified` (the enum is two variants with derived
weakest-link ordering; the extension is a variant, an `as_str` arm, and a
lineage counter key in `Applied`). `rust.fixRound` records at this tier only
for edits whose span and replacement come verbatim from a rustc/clippy
`MachineApplicable` `suggested_replacement`. Its classifier-synthesized
proposals (add-use, visibility-bump) floor at `syntax_only`.

Reasoning: the tiers are authorship lineage, not outcome guarantees.
`lsp_verified` means the server authored a semantics-preserving
transformation; rustc suggestions are compiler-asserted-to-compile, which is
stronger than a tree-sitter guess and weaker than semantics-preserving (the
compiler suggests what compiles, not what was meant). Flooring
compiler-authored bytes at `syntax_only` would erase exactly the sourcing
distinction the ledger exists to preserve, and the digest-based recognition
handles per-change tiers for free. The terminal `cargo check` in every
recipe remains the outcome gate regardless of tier.

Rejected: (a) floor at `syntax_only` (loses honest sourcing on the primary
rust repair path); (b) treat rustc as full `lsp_verified` authority
(conflates "compiles" with "semantics preserved", devaluing the top tier);
(c) revive `indexed_hints` as a ledger tier (v2 deliberately collapsed
hints into the floor; that honesty model stays).

### 8.2 RX-V1 channel: `ToolArgDefaults` via binding-side `lookup`, not input-merge

Decision: the transport is `ToolArgDefaults`, but consumed by a new public
`lookup(tool, param)` accessor on the table, not through the seam's
input-merge. The binding declares no `acknowledge_*` schema param (a cell
passing one gets an error naming the dispatch channel), queries the table
host-side, and records `operator_opt_outs_used` on the applied EditSet's
lineage when a grant is present. The isolate CLI grows a defaults flag
(`ToolArgDefaults::parse_map` already exists) so probes can exercise the
granted path.

Reasoning: grounding in `registry.rs` showed the seam merges defaults into
tool input before the call and attaches the `ToolArgRider` to the result
afterward, so an input-delivered flag is indistinguishable from a
cell-authored one at decision time: the confirm theater the trust model
forbids, detectable only by noticing an absent rider entry. The lookup path
makes cell authoring a schema error and operator grants mechanically
auditable, with a one-method addition to `bro-tools`.

Rejected: a dedicated dispatch field for RX-V1 flags (more legible, but new
machinery for two consumers; revisit if a third flag family appears).

### 8.3 `rust.extractItems` knob boundary: synthesis shape, never analysis gating

Decision: ship five knobs: `with_local_deps`, `section`,
`merge_into_existing_target`, `use_decl_visibility`, `use_decl_items`.
Capture/borrow analysis runs always and reports in `findings`, never
knob-gated. The acceptance test for any future knob, which also requires
probe evidence per the evolve-through-probes rule: a knob is legitimate iff
it selects among synthesis shapes the same host analysis produces either
way; it is illegitimate iff it gates whether analysis runs or encodes
multi-step orchestration (which is a recipe).

Reasoning: this is the precise form of the v2 critique ("parameters that
are really function calls"). `java.extractClass`'s `wrappers`/`wiring` are
shape knobs and are fine; v1's `deep_analysis` toggle was an analysis gate
and died at compaction. Scoring the candidates: `with_local_deps` selects
"move the exclusive closure" vs "report it in leftovers" with the dependency
graph computed regardless (legitimate); `section` is an addressing mode
(legitimate); per-item visibility maps are orchestration (rejected: run
`rust.setVisibility` after); any `deep_analysis`-style toggle is rejected
categorically.

### 8.4 `lsp.assist`: free-form list-then-apply, three mechanical guards

Decision: no allowlist. Bare `lsp.assist({span})` returns the server's
offered actions at the span (kind/title/index, capped); `select` applies
one. Guards: snippet-edit actions are flattened or refused with a named
hint; command-returning actions are refused with a pointer at
`lsp.executeCommand`; result lists are capped per the isolate-heap
discipline.

Reasoning: the authority boundary is the span: `codeAction` returns only
what the server offers at that span, contextually valid by construction, so
a filter selects from the server's own menu and an allowlist would only fork
that menu into a hand-maintained constant that rots every rust-analyzer
release. Wrong-assist selection is a wrong-intent failure, the same class as
`lsp.rename` to a bad name, which the trust model already answers with
detection (bounce findings, parse validation, the terminal compile gate)
rather than prevention. The real hazards are mechanical (snippet edits,
command results, payload size) and they get mechanical guards. The
list-then-apply shape also matches the "wide toolboxes stay compact indexes"
convention: the server is the catalog.

Rejected: an allowlisted assist set (rot without an invariant).

### 8.5 `analysis.*` bilingual with language tags and the divergence rule

Decision: rust reductions land in `analysis.*`. Three legibility
disciplines: the namespace index line carries the language tag per verb;
`analysis.describe` takes an optional language filter; and the divergence
rule: a verb name is shared across languages only when its contract is
structurally identical (`references` qualifies: same question, two
grammars), otherwise the verb gets a language-specific name.

Reasoning: the namespace encodes question shape and compute placement
(reductions run Rust-side; raw intermediates never enter the isolate heap),
not language. An agent asking a structural question should have exactly one
place to look, and splitting rust reductions into `rust.*` would break the
two-tier story the heap-safety discipline hangs on. The muddiness risk is
real but addressed by tags and the divergence rule rather than by namespace
surgery.

Escape hatch: if probing shows agents misfiling rust questions against Java
verbs (or vice versa), split after the first two rust reductions land, while
the move is cheap.

### 8.6 rust-analyzer build data on lane checkouts: host target dir + shim-stripped spawn env

Decision: v1 keeps rust-analyzer on the HOST and makes its child builds
host-consistent. When an `lsp.*` session's root is under a lane checkout
(`~/lanes/<name>/...`, canonicalized) or `BRO_LSP_RA_HOST_BUILD=1` is set,
bro-lsp spawns rust-analyzer with a scrubbed environment: (1) PATH with the
lane shim dir (`~/.lane/shims`, overridable via `BRO_LSP_LANE_SHIM_DIR`)
removed, so `cargo`/`rustc` resolve to the host rustup proxies; (2)
`CARGO_TARGET_DIR` set to a host-local per-root dir
(`BRO_LSP_RA_TARGET_DIR` or `<dirs::cache_dir>/blackbox/ra-target/<sha256(root)[:16]>`, i.e.
`~/Library/Caches/blackbox/ra-target/` on macOS, `~/.cache/blackbox/ra-target/` on Linux);
(3) `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` unset, so a sccache lane
shim cannot re-enter the pod. v2 (recorded, not built): pod-side
rust-analyzer with a bidirectional URI translation layer. Rejected:
documenting rust `lsp.*` as unsupported in lanes (strands the semantic tier
exactly where agents work); sharing the lane `target/` between pod and host
(the proc-macro dylib format mismatch is the original bug).

Reasoning: rust-analyzer derives build data by running `cargo metadata` and
`cargo check` (flycheck) as child processes resolved through its own PATH
with cwd at the workspace root. On a lane checkout two things conspire
against the host server: the cwd-keyed shim routes those children back into
the Linux pod, and the pod-built `target/` holds ELF proc-macro `.so` files
the host proc-macro server cannot load. The two knobs that matter (which
cargo runs, where artifacts land) are both environment-level, so scrubbing
the spawn env fixes the whole chain with no initializationOptions surgery,
no URI translation layer, and no fight with user `rust-analyzer.toml`
config. The per-root host target dir costs one cold host-side build (proc
macros + build scripts; minutes on a cold cache, incremental after) and can
never collide with pod artifacts. Path detection keeps the common case
zero-config; the explicit env var covers non-lane NFS/sshfs layouts.

Related readiness fix (gap-eeeab3bc, phase 2.0): `observe_rust_analyzer_status`
must evaluate the `health` field (`error` before the `quiescent`
early-return, message carried into detail) so a broken workspace load fails
closed instead of marking Ready and serving silent nulls.

## 9. Migration close-out (part of "complete", not an afterthought)

- `sm-refactor-rust` currently teaches the v1 `bbox_refactor_plan` API in
  exhaustive detail; rewrite it against this surface once phase 1 lands (the
  strangler's "this step *is* the migration" move).
- New `prompts/RUST_REFACTOR_DELEGATION.md` mirroring
  `JAVA_REFACTOR_DELEGATION.md`: toolbox pointer, canonical flows (§5
  recipes), brief template, footguns, verify loop.
- The ~22 rust atoms (`rust-split-god-impl`, `rust-state-extract`,
  `rust-trait-from-impl`, `rust-error-migrate`, ...) re-point onto these
  bindings; atom input schemas keep RX-V1 passthrough, never defaults.
- When the MCP retirement merges, delete the four LSP-backed v1 rust kinds
  and their `bbox-lsp` plumbing from `bbox-refactor` (§2.5).

## 10. Phasing

1. **Substrate**: `build.gate` rustc-JSON diagnostics; `rust.fixRound` port;
   `analysis.*` language plumbing + `implPartition` / `topLevelDeps` /
   rust-mode `references`; `lsp.references`; the RX-V1 dispatch channel
   (§2.4, including the `ToolArgDefaults::lookup` accessor); `rust_transforms/` module skeleton + `rust.describe`.
2. **Daily drivers**: `rust.extractItems`, `rust.extractImplMethods`,
   `rust.moduleWiring`, `rust.setVisibility`, `rust.inlineModToFile`,
   `rust.organizeImports`, `rust.rewriteModuleCallers`. After this phase,
   file/module/god-impl splitting runs end-to-end with compiler-gated
   repair. **Dogfood probe**: split `java_transforms.rs` (698K of rust)
   using only this surface, then run the RETRO prompt on the probe session.
3. **Phase 2.0 (RA substrate, inserted after probing)**: readiness
   `health`-field ordering + `lsp.status` detail surfacing (gap-eeeab3bc);
   lane-aware RA spawn env per §8.6 (gap-8637bb39); orphan RA child reap on
   errored session shutdown. Without this the semantic tier cannot be
   validated where agents actually work.
4. **Semantic tier**: `bro-lsp` `codeAction`/resolve -> `lsp.assist`;
   `lsp.definition`; `rust.extractTrait`,
   `rust.moveStructFields` + `rust.updateCallers` +
   `rust.addDelegateField`, `rust.migrateErrorType`,
   `rust.migrateTypeUsages`, `rust.liftToFree`. SSR re-route (probed
   2026-07-19): RA 1.96 answers `executeCommand rust-analyzer.ssr` with "No
   delegateCommandHandler", so SSR folds into `lsp.assist` via codeAction;
   the executeCommand WorkspaceEdit conversion is kept generic, consults
   `executeCommandProvider.commands` from server capabilities, and fails
   closed on unregistered commands.
5. **Campaign scale + close-out**: `rust.extractCrateScaffold` + crate-peel
   recipe, `analysis.workspaceDag`, `toml.ensureTable`,
   `rust.migrateStringFieldToEnum`, `rust_match_arm_to_strategy` disposition;
   `sm-refactor-rust` rewrite, delegation prompt, atom re-point.

Phases 1-2 are where daily rust development lives; phases 3-5 make the
surface complete. Each phase ends with an isolate-probe validation pass and
gap-filed friction, not with a doc claim.

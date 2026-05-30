---
title: "Unified Code Synthesis Model"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - refactor-tools
  - macros
  - code-navigation
  - java
tags:
  - refactor-tools
  - macros
  - code-navigation
  - mcp
  - java
  - transaction
date: 2026-05-25
updated: 2026-05-30
status: "partial — Phases 0–6 + v1 macro + guice library shipped; Phase 7 (Vaadin + jOOQ dissolution) remains"
brief: "Unifies slice, refactor, code-nav, and macro tooling into one force-amplifier model: a shared RefactorPlan IR + transaction, a thin data-defined macro layer that lowers to it, probe operations that bind to the code-nav/LSP query substrate, and a program to dissolve framework-specific Rust refactor primitives into macro ontology."
---

# Unified Code Synthesis Model

## Implementation status (2026-05-30)

**Partial — the model is built; only the bulk dissolution remains.** Grounded
against code (file:line as of 2026-05-30):

- **Phase 0** (Java first-class code nav) — **shipped**, see
  [java-code-nav.md](java/java-code-nav.md) (archived).
- **Phase 1** (transaction additions) — **shipped**: `FileCreate` and the
  validation steps are in the `refactor` transaction
  (`src/macros/model.rs`, `sidecar_backend.rs`).
- **Phase 2** (macro core) — **shipped**: eight `macro_*` MCP tools registered
  (`macro_list/describe/validate/plan/apply/run/register/unregister` in
  `src/tools/macros.rs`); the `explain` verb floated in the roadmap was not
  built. Data model + bounded probe DSL + lowering live in `src/macros/`
  (`model.rs`, `probe.rs`, `expr.rs`, `planner.rs`, `registry.rs`).
- **Phase 3** (Java backend sidecar) — **shipped**: `JavaMacroBackend` with a
  fail-closed `UnavailableBackend` default (`src/macros/backend.rs`,
  `planner_ctx.rs`), the sidecar protocol/client (`java_sidecar.rs`,
  `java_sidecar_protocol.rs`, `sidecar_backend.rs`), `BLACKBOX_JAVA_BIN` spawn
  gating.
- **Phase 4** (probe bindings) — **shipped** (`src/macros/probe.rs`).
- **Phase 5** (dissolution proof) — **shipped**:
  `builtin.java.vaadin.ensure_provider_bindings` registered;
  `vaadin_provider_bindings.rs` deleted.
- **Phase 6** (Lombok dissolution) — **shipped**: `builtin.java.lombok`
  registered; `lombokify.rs` deleted.
- **v1 macro** `java.add_service_boundary` — **shipped** (registered +
  `java_service_boundary_tests.rs`).
- **`builtin.java.guice`** — **shipped** (`registry.rs`), even though the body
  below (Phase 5 note + Phase 7) still describes it as pending. Treat the
  registry as authority.

**Remaining = Phase 7**: the ~25 `java_vaadin_*` / `java_jooq_*` plan kinds are
still live in the Rust refactor registry (`src/refactor/`) and have not been
dissolved into macro ontology; the jOOQ analysis-kind shape question
(§"Analysis-only kinds") is still open. The body below predates these landings
— read it as the design record, not current state.

## Thesis — the force-amplifier lineage

Blackbox's structural code tools all exist for one reason: **keep code off the
LLM's context window while still letting it drive correct, large mechanical
changes.** The LLM spends tokens on *intent and shaping* — which boundary to
create, which symbol to move, what the new contract is — not on transcribing
hundreds of lines of source through the transcript.

Three tools already embody this, and a fourth completes the set:

| Tier | Tool family | Force-amplifier property | Token property |
|---|---|---|---|
| **Locate** | `bbox_code_query` / `code_symbols` / `code_refs` (+ semantic tier) | one call → N match sites + pre-filled next-call args | results are *refs/positions*, not bodies |
| **Move** | `bbox_slice_*` | cut/copy/move/replace by selector | content never enters context ("off the clipboard") |
| **Transform** | `bbox_refactor_*` | one call → N call-site rewrites, semantics-preserving | symbol-addressed, not text-pasted |
| **Synthesize** | `macro_*` (new) | one call → probe + emit + rewrite + wire, composed | typed knobs in; generated bodies never round-trip the LLM |

Slices keep moved text off the clipboard (`src/slices.rs`); refactor keeps the
N rewritten call sites out of the transcript (`src/refactor/mod.rs`); code-nav
returns *refs* with handoff args instead of file dumps (`src/code_nav/mod.rs`).
Macros are the missing **synthesis-side amplifier**: the user defines a set of
discoverable, typed macro inputs, and the LLM uses them to shape a
mechanical/correctness advantage — adding code to existing components,
extracting with transformation, or minting net-new components — without ever
typing the interface, the impl skeleton, the binding, or the import management
into context.

This doc is the umbrella. It grounds the macro layer against the actual
transaction code and extends it with the code-nav/LSP probe substrate and the
program to dissolve framework-specific Rust primitives into data. The Java
semantic-navigation capability it depends on is specified in its sibling,
[Hoisting Java to First-Class Code Navigation](java/java-code-nav.md).

## Grounding corrections

The original macro proposal made architectural assumptions that closer reading
of the code reverses. These corrections are load-bearing for the whole design.

### Whole-file replacement already exists (it is not a new edit type)

The original doc framed an either/or: a sidecar (OpenRewrite) returning
whole-file transformed source would need *either* a diff-back-to-byte-ranges
step *or* a new first-class whole-file replacement edit. **Both are
unnecessary.** Whole-file replacement is already a degenerate full-span
`TextEdit` and already flows through the exact safety path:

- `plan_write_file` (`src/refactor/mod.rs:3693`) emits a single
  `TextEdit { byte_start: 0, byte_end: source.len(), replacement: whole_source }`.
- `plan_ensure_toml_table` (`:3733`) does the same for a structured rewrite —
  generate the entire new file, ship it as one `0..len` edit.
- `apply()` (`:1633`) runs the identical gauntlet for these as for a 3-byte
  surgical edit.

A sidecar that returns transformed whole-file source is therefore wrapped as a
full-span `TextEdit` against the preimage sha and inherits every safety
property for free. Diff-back-to-byte-ranges is demoted to a *later
reviewability optimisation* (nicer `git diff`, finer conflict reporting), not a
correctness requirement and not a v1 blocker.

One caveat the original under-stated: `FileEdit.new_text` (`:823`) is **not**
the write channel — its doc comment says *"Never written to disk"*; it is the
RX-A2 blocked-plan preview artifact. The write channel is and remains
`FileEdit.edits: Vec<TextEdit>`.

### `apply()` is already the shared transaction

The original called for "factoring an `EditTransaction` trait out of refactor
apply." Grounded against the code, that is the wrong verb. `apply()` already
*is* the shared transaction, and the slice tools already proved macros can
consume it without a new engine: every `bbox_slice_*` mutation builds a
`RefactorPlan` via `plan_from_edits` and calls `refactor::apply()`
(`src/slices.rs`). Slices did not extract anything — they **produced the
existing plan shape**. That is the template macros copy.

The guards `apply()` already enforces, in order (`src/refactor/mod.rs:1633`):

1. `confirm=true` gate (`:1634`).
2. **G15** cross-worktree refusal — compares git toplevels of plan anchor vs
   apply cwd, fails closed even when `cwd` is omitted; `force_path=true` is the
   explicit bypass (`:1669`).
3. **RX-A2** `plan_blocked` refusal when `plan_status == Blocked` (`:1722`).
4. Registered-path check (`ensure_path_in_registered_project`, `:1738`).
5. Dirty-worktree check (`ensure_git_clean_for_path`, `:1742`).
6. Preimage sha verification (`read_original_for_edit` against
   `FileEdit.original_sha256`).
7. Snapshot into `originals` for multi-file rollback (`:1732`).
8. Parse validation of the rewritten content **before any write**
   (`validate_rewritten_files`), then atomic write with rollback from the
   per-file snapshots on failure.

(Ordering above is representative, not byte-exact: snapshots are captured
per-file alongside the preimage reads, and validation runs pre-write — not as a
post-write `ValidationStep` loop.)

Multi-file atomicity and rollback are therefore **already present** — but only
if a macro lowers to **one** `RefactorPlan`, not N orchestrated refactor calls.
This is the single load-bearing reason to compile-to-IR rather than chain tool
calls.

### Framework taint is already cleanly quarantined

The dissolution program (below) is viable because the code is already
well-partitioned. Generic DI probing, `@Inject`/`Provider<T>` field detection,
field synthesis, and import management live in `src/refactor/java/di_plumbing.rs`
and are framework-agnostic. Framework policy is isolated in `vaadin_*.rs` and
the `G7` wiring-mode enum in `extract_class.rs`, with **zero seepage** into the
generic modules.

## The shared IR and transaction

**`RefactorPlan` is the intermediate representation. `MacroPlan` lowers to it.
`refactor::apply()` is the one writer.**

```text
MacroDefinition + typed inputs + probes
   → MacroPlan          (review artifact: operations, backends_used,
                          semantic_status, refusals, operator_opt_outs_used)
   → lowers to →
   RefactorPlan         (file_moves, edits: Vec<FileEdit>, validations)
   → refactor::apply()  (the sole writer; all guards live here)
```

Consequences:

- **`macro_apply` is `refactor::apply` with a macro-shaped envelope** — the
  ergonomic confirmation/provenance wrapper that preserves macro audit fields
  (`operator_opt_outs_used`, `backends_used`). The slice precedent shows the
  envelope is ~60 lines (`plan_from_edits` + `finish_mutation`). There is no
  separate apply engine to be tempted by.
- **Sidecar whole-file output → full-span `TextEdit`** against the preimage sha.
- **Cross-file atomic apply + rollback** comes free *because* the macro lowers
  to one plan with N `FileEdit`s.

### Transaction additions genuinely required

Four additions, all incremental to existing types — no new subsystem:

1. **`FileCreate` as the one new transaction primitive.** Today new-file
   emission rides a 0-byte-preimage `FileEdit` (`sha256_hex(&[])`, as in
   `leaf_plans.rs` interface generation). That works but is implicit. A macro
   minting N files (interface + impl) wants a first-class
   `FileCreate { path, content, fail_if_exists }` so the guard is "refuse if
   exists" rather than "preimage happened to be empty." Sits beside the existing
   `FileMove` (`src/refactor/mod.rs:826`).
2. **`ValidationStep::LspNoDiagnostics { path }`** — backs `lsp_verified`
   macro/refactor output. Additive to the enum (`:882`), today
   `TreeSitterNoErrors`-only.
3. **`ValidationStep::CommandSucceeds { ... }`** — backs the macro command
   policy (`mvn`/`gradle`/`javac`/test), gated by origin (operator vs agent
   allowlist), distinct from the cargo-only refactor-atom allowlist (RX-V2).
4. **`operator_opt_outs_used` on the durable `RefactorPlan`.** RX-V1 requires
   this audit field on the durable plan, "not just the summary." The current
   `RefactorPlan` struct (`src/refactor/mod.rs:891`) has **no such field** — the
   existing code threads opt-out audit through wrapper/leftover side channels.
   Macros surface it on `MacroPlan`, but the durable record must live on the
   lowered `RefactorPlan`; adding the field is part of this transaction work.

## Macro layer

The macro layer owns only the parts that are genuinely *data*: registration,
input schemas, the probe/refusal DSL, planning, provenance, and the lowering to
`RefactorPlan`. It does not own Java parsing or source printing, and it does not
own search.

### Data model (grounded)

`MacroDefinition` — a registered recipe (data, not plugin code):

```json
{
  "id": "java.add_service_boundary",
  "version": "1",
  "language": "java",
  "scope": "builtin|user|project",
  "title": "Add Java service boundary",
  "inputs_schema": {},
  "effects": ["creates_type", "changes_dependency_injection"],
  "authority_gates": [],
  "probes": [],
  "operations": [],
  "validations": [],
  "refusals": []
}
```

Builtins ship with Blackbox; project macros live under `.bbox/macros/` so teams
review them like source; user macros live in operator config.

`MacroPlan` is the review artifact and is **not** a `RefactorPlan` — it may
embed delegated refactor-plan summaries, but its first-class shape is "this
recipe will create/modify these artifacts using these backends," and it carries
`backends_used`, `operator_opt_outs_used`, and `semantic_status`. It lowers to a
single `RefactorPlan` at apply time.

### Operation vocabulary

Small and fixed — the Java backend handles Java detail:

- `probe` — inspect project style, packages, symbols, DI conventions, build
  commands. **Binds to the code-nav/LSP query substrate (below); does not
  reimplement search.**
- `emit` — generate a new artifact via a typed backend op (JavaPoet → `FileCreate`).
- `rewrite` — transform existing source (OpenRewrite → `TextEdit`s/full-span).
- `delegate_refactor` — call an existing `bbox_refactor_*` kind when that is the
  right primitive.
- `validate` — parse / organize imports / compile / test (→ `ValidationStep`).
- `record` — structured residue / follow-up notes.

### Semantic status

Reuse the refactor `SemanticStatus` ladder (`src/refactor/mod.rs:835`):
`syntax_only`, `indexed_hints`, `lsp_verified`, `lsp_verified_partial`. Macro-only
additive statuses: `template_only` (generated text unparsed/unverified) and
`mixed` (per-operation statuses differ). A backend that requires an AST/LSP
check and finds it unavailable **fails closed** — it never silently downgrades
to `template_only` (mirrors RX-V3).

### Bounded expression grammar

Deliberately small so project macros stay human-writable: dotted-path lookup
into `inputs`/probe results; equality/existence/list-membership predicates;
`${path}` interpolation in string fields; `all`/`any` over bounded predicate
lists. No arithmetic, loops, user functions, or arbitrary boolean programs.

## Probe stitching — macros over the nav substrate

This is the integration the original proposal missed. Macro probes
(`java.search.type`, `java.search.member`, `java.search.project_text`) are
**not new search code inside the macro engine.** `src/code_nav/` is already a
query engine with a purpose-built handoff seam for exactly this.

A macro `probe` operation is a thin, named, parameterized binding over the
existing nav tools, capturing the result into a named slot that `emit`/`rewrite`
operations consume:

- Syntactic probes bind to `bbox_code_query` (raw tree-sitter S-expressions),
  `bbox_code_symbols` (project-wide inventory), `bbox_code_refs` (calls/imports/
  fields). These return `semantic_status: syntax_only`.
- Semantic probes bind to the new Java semantic tier — `References`,
  `Implementation`, `WorkspaceSymbol`, `DocumentSymbol`, `Hover` over jdtls —
  returning `lsp_verified`, fail-closed per RX-V3. Specified in
  [java-code-nav.md](java/java-code-nav.md).

The bridge from query → mutation already exists: `CodeRefactorHandoff`
(`src/code_nav/mod.rs:635`) pre-fills the next `bbox_refactor_status` /
`bbox_refactor_project_refs` call on every result. Macro probe slots reuse the
same seam.

### Prefilter-then-resolve

The bounded-cost idiom that both macros and bare plan-shaping LLMs should
follow (and that `rust_ra_classify_callbacks` already demonstrates: tree-sitter
finds call sites → `GotoDefinition` classifies each):

> **Narrow to candidates with tree-sitter `code_query` (milliseconds, no LSP),
> then resolve only the candidates with jdtls (`Hover`/`DocumentSymbol`).**

This is the correct resolution of the original `java.search.member` ontology gap
("find methods by annotation and erased/generic return type"):

- `@Provides`-annotated methods + syntactic return shape → `code_query`
  S-expression. Already possible.
- erased/generic *resolved* return type on each candidate → `Hover` on the
  bounded candidate set, **not** the whole project (jdtls cold-start is 60s; see
  java-code-nav cost model).

The gap splits cleanly: the syntactic half lives in `code_nav` (reusable by any
caller), the semantic half is a thin generic jdtls caller, and **neither
mentions Guice/Vaadin** — the genericity test passes.

## Dissolving Rust primitives into macro ontology

The deeper goal: framework-specific behavior currently hard-coded as Rust
refactor plan kinds (Vaadin, Guice) should become **system-default or example
macro libraries** — data and reusable recipes, reviewable in-repo, scoped to a
project. The Rust core stays generic.

### The boundary

- **Refactor core**: generic, reusable source transformations + transaction
  safety.
- **Language backends**: Java parsing, source generation, source rewriting,
  import management, formatting (OpenRewrite + JavaPoet).
- **Macro libraries**: project/library/framework behavior expressed as data and
  recipes.

**Hard rule:** project/library-specific primitives do not belong in the Rust
core engine. "Insert this Java member from a template," "find methods with this
annotation and return type," "add these imports if absent" are acceptable core/
backend operations. "Add a Guice binding," "register a Vaadin route" are not —
those are macro-library data or installable JVM recipe-pack code.

### Recipe packs are code, not data

The boundary is authority and provenance, not sandbox security. A macro
definition must not make executable backend code look like ordinary
project-scoped recipe data. Recipe packs are artifact-catalog-versioned backend
extensions (vetted OpenRewrite recipes) with an explicit operator install/
approval step; project macros reference them by id/version. This keeps
framework-specific code out of Rust core while making code-bearing macro
dependencies auditable.

### Migration ladder

For each existing framework-specific Rust helper:

1. **Keep the Rust helper only if it passes the genericity test:** could a
   non-Guice/non-Vaadin macro plausibly use the same transformation or backend
   primitive? "Complex" is not enough.
2. Extract the library-specific policy into a macro definition.
3. Attempt to express the old behavior with the current macro ontology + backend
   operations.
4. For every part that cannot be expressed, **record an ontology gap before
   adding code.** The gap names the missing capability, classifies it, and
   argues its genericity.
5. Fill only generic gaps in the engine/backend, or ship framework-specific
   logic as an installable JVM recipe pack owned by the macro library. Never add
   a Guice/Vaadin-specific primitive to Rust core.
6. After the parity proof, DELETE the old refactor plan kind and all atom/eval/doc
   references — no compatibility wrapper (it fabricates legacy concerns that don't exist).

### Ontology gap record

```json
{
  "source_plan_kind": "java_vaadin_provider_binding_generation",
  "missing_capability": "find methods by annotation and erased/generic return type",
  "classification": "java_backend_probe | java_backend_operation | semantic_query | transaction_capability | recipe_pack | out_of_scope",
  "genericity_argument": "needed for Guice @Provides, Spring @Bean, JAX-RS routes, project-local annotation conventions",
  "not_library_policy": "does not mention Guice, Vaadin, UI, or VaadinSession",
  "candidate_surface": "java.search.member (syntactic prefilter) + Hover (semantic resolve)",
  "fallback": "recipe_pack"
}
```

### Worked example — `java_vaadin_provider_binding_generation` → `builtin.java.vaadin.ensure_provider_bindings`

The Rust primitive was dissolved after parity proof (P5-2/P5-3). The Rust kind,
its atom `java-vaadin-provider-binding-generation`, and all eval/doc references
were deleted with no compatibility wrapper.

**Generic substrate gaps filled to enable the migration:**
- `ProbeSpec::ProjectText` — project-wide file-content scan (needles, match_mode
  any/all, language filter, path_contains scoping, normalization) enabling
  framework-marker detection and whitespace-normalized idempotency checks without
  a Rust-coded tree-sitter walk.
- `when: Option<Predicate>` on `MacroOperation::Emit` / `Rewrite` — per-op guard
  that skips the backend call when the predicate is false, enabling idempotency
  without a separate refusal rule.

**Macro data owns** (was Rust in `vaadin_provider_bindings.rs`): the required
bindings (`Provider<UI>`, `Provider<VaadinSession>`); imports
(`com.google.inject.Provides/Provider`, `com.vaadin.flow.component.UI`,
`com.vaadin.flow.server.VaadinSession`); provider method names/bodies
(`return UI::getCurrent;`, `return VaadinSession::getCurrent;`); refusal policy
(Spring detected → refuse; non-Guice module; Vaadin not detected).

**Generic backend/probe capabilities it binds to:** `java.search.type` (find the
module type → `bbox_code_symbols` / `WorkspaceSymbol`); `java.search.project_text`
(detect framework markers with build-dir exclusions → `ProbeSpec::ProjectText`
which prunes `target/build/.gradle/node_modules`);
`java.template.insert_member` (OpenRewrite `JavaTemplate`); idempotency via
`ProjectText` + `when`-guard on each rewrite op.

No gap may mention `UI`, `VaadinSession`, `@Provides`, or Spring refusal as
*engine* concepts. Those stay macro-library data.

### Disposition of current Java refactor pieces

| Current Rust (`src/refactor/java/`) | Disposition |
|---|---|
| `di_plumbing.rs` — `@Inject`/`Provider<T>` detection, field synthesis, import edits | **Generic backend ops + probes.** Keep, expose as backend operations. |
| `extract_class.rs` extraction mechanics (captured deps, residue) | **Generic refactor.** Keep as `delegate_refactor` target. |
| `extract_class.rs` `G7` Guice wiring-mode enum (`constructor_args`/`guice_field_inject`/`manual`) | **Library policy.** → macro data (wiring choice as input/refusal). |
| `migrate_receiver.rs`, `split_provider.rs`, `replace_static_ref.rs` | **Generic DI-aware rewrites.** Keep as `delegate_refactor` targets. |
| `leaf_plans.rs` `extract_java_interface`, `add_java_implements`, `java_lsp_organize_imports` | **Generic.** Keep; callable as backend/delegate ops. |
| `vaadin_provider_bindings.rs` | **Dissolved → `builtin.java.vaadin.ensure_provider_bindings`.** Rust kind deleted after parity proof; no compat wrapper. Generic substrate gaps filled: `ProbeSpec::ProjectText` + `when`-guard on mutating ops. |
| `vaadin_view_synthesis.rs`, `vaadin_component_extract.rs`, `vaadin_grid_extract.rs`, `vaadin_dialog_extract.rs`, `vaadin_navigation_helper.rs`, `vaadin_route_access.rs` | **Library policy (mutation).** → `builtin.java.vaadin` macros (route collision, access-policy) over generic emit/probe. Not yet dissolved. |
| `vaadin_route_inventory.rs`, `vaadin_static_ui_audit.rs`, `vaadin_view_structure.rs` | **Library policy (analysis-only).** → `builtin.java.vaadin` probe macros, or stay as `code_nav` probes (see analysis-kind note below). Not yet dissolved. |
| `jooq_analysis.rs` — 6 analysis kinds (`codegen_config_inventory`, `query_structure_analysis`, `dsl_context_audit`, `projection_mapping_analysis`, `raw_sql_fragment_audit`, `generated_record_boundary_analysis`) | **Library policy (analysis-only).** Pure read-only inventory/audit, tree-sitter + text scan, no LSP. → `builtin.java.jooq` probe macros over `ProbeSpec::ProjectText` + `code_query`. Not yet dissolved. |
| `jooq_mutations.rs` — 9 dissolvable mutation kinds (`raw_sql_fragment_rewrite`, `extract_query_method`, `extract_query_object`, `synthesize_query_method`, `synthesize_dto_projection`, `condition_builder_extract`, `generated_record_boundary_rewrite`, `codegen_extension_apply`, + nesting-detection half of `transaction_boundary_normalize`) | **Library policy (mutation).** Operator-supplied names/types, tree-sitter insertion points, text-emit templates — no jOOQ-specific backend. → `builtin.java.jooq` macros over generic emit/rewrite/`delegate_refactor`/`insert_member`. Not yet dissolved. |
| `jooq_mutations.rs` — `java_jooq_extract_repository` | **Recipe pack / operator-policy gap.** Already blocked in v1; needs DSLContext-scope, read/write, transaction-boundary, and public-API policy fields that are not expressible in current macro ontology. Classify as `recipe_pack` before any code. |
| `jooq_mutations.rs` — `transaction_boundary_normalize` (policy half) | **Partial.** Nesting detection is generic; the flatten-vs-annotate / savepoint-preservation semantics are jOOQ tx-callback policy → macro data + operator opt-in flag, but record an ontology gap if a generic rewrite-rule template cannot carry it. |
| `lombokify.rs` — `lombokify_java_class` | **Dissolved → `builtin.java.lombok`** (Phase 6). Rust kind, `java-lombokify` atom, `sm-refactor-java-lombokify` memory, `pojo-modernize` workflow, and lombokify tests all deleted after parity proof; no compat wrapper. The generic `formal_parameters` helper was preserved (moved to `method_params`). Generic substrate added: `ForEach` fan-out, `DeleteMember` / `insert_class_annotation` / `insert_field_annotation` / `prune_unused_import` ops, an `analyzeClass` structural probe, and inputs-schema defaults. All Lombok policy (`@Data`/`@Value` collapse, annotation names, boolean skip/bridge/rename, declines) is macro data. Parity proven on 12 scenarios. |

### Remaining dissolution scope (the holes Phase 5 left)

Phase 5 dissolved `vaadin_provider_bindings.rs`; Phase 6 dissolved
`lombokify_java_class`. The disposition table above enumerates every
framework-specific Rust kind still in the tree. Counts as of this revision:
**9 Vaadin kinds and 17 jOOQ kinds remain** hard-coded as Rust plan kinds
(Lombok is now dissolved). An earlier table understated this by omitting the
entire jOOQ family and Lombok; that omission is corrected above.

Run each through the genericity test (ladder step 1) before writing code:

- **Vaadin analysis + extract/synthesize kinds** — same shape as the proven
  `ensure_provider_bindings` migration (probe + guarded emit/rewrite). Lowest new
  substrate risk; these are the natural Phase 7 batch.
- **jOOQ analysis kinds (6)** — pure read-only inventory/audit. They map to probe
  macros, but see the analysis-kind note below: a read-only "report" macro is a
  different shape than a mutating macro that lowers to a `RefactorPlan`.
- **jOOQ mutation kinds (9 dissolvable)** — operator-supplied names/types +
  tree-sitter insertion + text-emit. No jOOQ-specific backend; `delegate_refactor`
  + `insert_member` + `when`-guards cover them. `extract_repository` stays a
  recipe-pack/operator-policy item; `transaction_boundary_normalize` is partial.
- **Lombok (`lombokify_java_class`)** — **done (Phase 6).** Was the recommended
  entry point: single kind, syntax-only, no LSP, transformations generic across
  any annotation-codegen library (Immutables, AutoValue, MapStruct). It was not
  zero-gap — the gaps below were filled first, then it dissolved cleanly.

### Generic ontology gaps Lombok dissolution required *(filled in Phase 6)*

These are framework-agnostic (no Lombok name appears in them); they were filled
in the backend before expressing `lombokify` as macro data:

1. **`insert_class_annotation`** — add an annotation to a type declaration
   (class-level placement), with dedup against existing annotations. Today
   `java.template.insert_member` only inserts *inside* the class body.
2. **`insert_field_annotation`** — add an annotation immediately above a named
   field (per-field placement), for the case where a class-level annotation does
   not apply to all fields.
3. **`ensure_import`** — add an import with dedup and existing-wildcard awareness,
   and its inverse **`prune_unused_import`** (remove an import whose symbol is no
   longer referenced after edits). `ProbeSpec::ProjectText` can *detect* import
   presence today, but there is no generic emit op for managed import add/prune.

Member **deletion** (drop a method/field byte-range with trailing-newline
cleanup) is also required; confirm whether the slice/refactor layer already
exposes a generic delete the macro lowering can target, or whether a
`MacroOperation::Delete` is the minimal addition. The `@Data`/`@Value` collapse
cascade and boolean-getter strategy (skip/bridge/rename) are expressible as
bounded `when`-guards + an operator-supplied input — no new control flow beyond
the existing predicate grammar.

### Analysis-only kinds are a different macro shape (open question)

The jOOQ and Vaadin *analysis/audit/inventory* kinds are read-only: they emit a
structured report, not a `RefactorPlan`. The macro IR lowers to one
`RefactorPlan` and `refactor::apply()` is mutation-shaped. A read-only macro that
produces a JSON inventory either (a) lowers to an empty-edit plan with the report
in a `record`/residue channel, or (b) does not belong in the macro layer at all
and should stay a `code_nav` probe surface. Decide this before dissolving the six
jOOQ analysis kinds — do not force read-only inventory through a mutation-shaped
plan just to retire the Rust kind.

## Useful V1 — `java.add_service_boundary`

One real composite macro, not a catalog demo. Takes an existing caller and
creates a service boundary around one named operation.

Required inputs: `caller_file`, `caller_type`, `caller_method`, `service_name`,
`service_package`, `implementation_name`, `implementation_package`,
`guice_module`, `method_contract`, and either
`implementation_body` + `caller_replacement`, or a supported `behavior_source`
that delegates to existing refactor tooling.

Plan behavior:

1. **Probe** caller type, method ambiguity, package roots, DI style, existing
   constructors, Guice module shape (binds to code-nav syntactic + jdtls
   semantic where ambiguity needs resolving).
2. **Emit** the service interface (JavaPoet → `FileCreate`).
3. **Emit** the implementation class (JavaPoet → `FileCreate`).
4. **Rewrite** caller to inject the service dependency (OpenRewrite → `TextEdit`).
5. **Rewrite** caller method body → delegation.
6. **Rewrite** add the Guice binding.
7. **Validate** — organize imports on touched files; optional build/test command
   under the command policy.
8. **Record** residue as structured follow-up.

`behavior_source` is explicitly the later/harder mode. v1 supports: explicit
`implementation_body` + `caller_replacement`; delegation to an existing refactor
kind when the source shape matches; or refusal with
`error.behavior_move_unsupported`.

V1 refusal cases: caller method missing/ambiguous; service or impl type already
exists; Guice module not found/ambiguous; binding already exists; injection
style unclassifiable; public-API change without operator authority; backend
cannot produce parse-valid Java; backend unavailable (`error.backend_unavailable`,
fail-closed).

## Safety invariants

- Macro planning is read-only.
- Macro application is hash-guarded and rollback-capable (via `refactor::apply`).
- Backends are pure `EditSet` producers; they never write files directly.
- Project macros are data, not automatically trusted code execution.
- Recipe packs are operator-installed, artifact-catalog-versioned code.
- Public-API/destructive/transaction-boundary/runtime-behavior effects require
  explicit authority gates. Agents may pass through operator-supplied authority
  flags but must not infer them (RX-V1). Consumed flags are surfaced on
  `MacroPlan` and, per RX-V1, recorded durably on the lowered
  `RefactorPlan.operator_opt_outs_used` (a field that must be added — see
  Transaction additions), not on the summary alone.
- LLM-generated bodies are `template_only`/`mixed` unless parser/LSP/build checks
  validate them.
- RA/jdtls-backed probes fail closed on LSP unavailability (RX-V3); no silent
  syntactic downgrade for an operation that requested semantic verification.

## Integration roadmap

Phased so each phase is independently useful and testable.

- **Phase 0 — Java first-class code nav** (sibling doc dependency): hoist
  `lsp::convert` helpers, grow `initialize` capabilities, add semantic query
  callers + the `code_nav` semantic tier. See
  [java-code-nav.md](java/java-code-nav.md). *Independently valuable for refactor
  plan-shaping and bare LLM due-diligence even before macros exist.*
- **Phase 1 — transaction additions**: `FileCreate`; `ValidationStep::LspNoDiagnostics`
  and `CommandSucceeds`; the macro command policy (operator vs agent allowlist).
- **Phase 2 — macro core**: `macro_*` MCP tools (list/describe/validate/plan/
  apply/run/register/unregister/explain) with list-before-register; the data
  model; the bounded probe/refusal DSL; the lowering to `RefactorPlan`;
  `macro_apply` as the `refactor::apply` envelope.
- **Phase 3 — Java backend sidecar**: OpenRewrite (existing-source rewrite) +
  JavaPoet (generation) behind one `JavaMacroBackend` adapter, with a
  sidecar-owned small declaration model and fail-closed availability reporting;
  whole-file output → full-span `TextEdit`. The sidecar runs as an isolated
  subprocess pinned to the bundled `rewrite-java-21` grammar module and must be
  launched with a JDK 21 `java` binary (`BLACKBOX_JAVA_BIN`); the Rust client
  enforces this at spawn time via the `getCapabilities` `java_version` field.
- **Phase 4 — probe bindings**: wire macro `probe` ops to code-nav syntactic and
  jdtls semantic tiers via the `CodeRefactorHandoff` seam; codify
  prefilter-then-resolve.
- **Phase 5 — dissolution proof** *(done, one instance)*: shipped
  `builtin.java.vaadin.ensure_provider_bindings` (dissolving
  `vaadin_provider_bindings.rs`), recorded the `ProbeSpec::ProjectText` +
  op-`when`-guard gaps, deleted the old plan kind outright with no compat wrapper.
  This proved the *method*; it did not migrate the rest of the surface. A
  `builtin.java.guice` library was named here but not yet shipped.
- **Phase 6 — Lombok dissolution** *(done)*: filled the generic ontology gaps —
  `ForEach` fan-out (the engine was loop-free; the make-or-break gap),
  `DeleteMember`, `insert_class_annotation`, `insert_field_annotation`,
  `prune_unused_import`, a generic `analyzeClass` structural probe, and
  inputs-schema defaults — then expressed `lombokify_java_class` as the
  `builtin.java.lombok` macro (all Lombok policy as data, incl. `@Data`/`@Value`
  collapse and boolean skip/bridge/rename). Parity proven against the Rust kind
  on 12 scenarios; bulk dissolved into per-class macro invocation (orchestration,
  not baked in). Rust kind + atom + memory + workflow + tests deleted, no compat
  wrapper. Confirms the thesis: the obstacles were ontology-completeness gaps
  fillable generically, not reasons to keep library-specific Rust.
- **Phase 7 — Vaadin + jOOQ batches**: dissolve the remaining Vaadin
  extract/synthesize kinds (probe + guarded emit/rewrite, same shape as Phase 5)
  and the 9 dissolvable jOOQ mutation kinds; resolve the analysis-kind shape
  question before the six jOOQ analysis kinds; ship `builtin.java.guice` (the
  `extract_class.rs` `G7` wiring-mode enum → macro data). `extract_repository`
  stays a recipe-pack item; `transaction_boundary_normalize` policy half pending
  the rewrite-template gap.
- **v1 macro**: implement `java.add_service_boundary` with fixtures proving
  boundary creation in a Guice project and refusal of the ambiguous cases.

## Dependents and crosscuts

- **Sibling, hard dependency:** [Hoisting Java to First-Class Code Navigation](java/java-code-nav.md)
  (Phase 0; the semantic probe substrate).
- **Reuses:** [Context Clipboard Refactor Primitives](context-clipboard-refactor-primitives.md)
  (the slice→`RefactorPlan` precedent), [Refactor Tools](refactor-tools.md),
  [AST-Assisted Refactor Mechanization](ast-refactor-mechanization.md).
- **Affects:** [Java Refactor Tools](java/java-refactor-tools.md),
  [Vaadin Refactor Tools Proposal](java/vaadin-refactor-tools-proposal.md)
  (their framework-specific kinds are dissolution targets).
- **Corpus:** [Code Navigation](../corpus/code-navigation/code-navigation.md) hub.

## Open questions

- Do macro definitions enter the artifact catalog at v1, or project/user
  registries only?
- OpenRewrite-first with JavaPoet embedded, or two helpers behind one adapter?
- Should generated per-macro MCP tools be opt-in to avoid catalog bloat? (They
  are aliases over the generic `macro_*` tools regardless.)
- Diff-back-to-byte-ranges: when does review noise from full-span edits justify
  building it?

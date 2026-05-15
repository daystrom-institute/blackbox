---
title: "Rust Refactor Expansion - closing the Java capability gap"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
  - rust
tags:
  - refactor-tools
  - rust
date: 2026-05-10
revision: "rev 2 - applies codex-gpt55 review convergence"
status: "design proposal, pure design (no implementation phasing)"
brief: "Designs Rust refactor analysis and plan kinds to close the gap with Java deep-analysis workflows."
---

# Rust Refactor Expansion — closing the Java capability gap

Related: `../ast-refactor-mechanization.md`, `../refactor-compound-runs.md`,
`sm-refactor-rust`, `sm-refactor-java`

## Problem

The Rust refactor backend today is essentially **mechanical movement**: extract
items, extract impl methods, add/copy/rewrite mod/use/visibility, LSP rename,
organize imports. Apply gates on parse + hash + clean worktree. The
Java backend goes considerably further: `deep_analysis` produces
`captured_variables`, `external_calls`, `inherited_dependencies`; the planner
can rewrite remaining accessors through a delegate, emit getters/setters,
scaffold `// FIXME:` markers in generated targets, collapse boilerplate to
`@Data` / `@Value` via `lombokify_java_class`, migrate type usages, and
extract interfaces with implements-injection.

Where Java wins isn't grammar coverage. It's that **the planner reports the
dependency boundary as part of the plan and supplies targeted edits to close
it.** Every Rust split of any non-trivial size hits the same boundary:
methods read `self.<field>`, call `Self::<other>`, depend on trait-method
dispatch, or reach generics on the host impl. The current Rust toolkit will
happily split across files and leave the operator with a non-compiling
mess that `cargo check` surfaces minutes later.

This design proposes the Rust plan kinds and analysis surfaces needed to
give Rust feature parity with Java's `deep_analysis`-class operations,
**within the limits of what tree-sitter alone can prove**. Operations that
require resolving symbol bindings, trait dispatch, deref/autoref, macros,
type inference, or re-exports are explicitly delegated to rust-analyzer
(LSP-backed plan kinds) or to the compiler (a new `rust_compile_fix_round`
plan kind fed by `cargo check --message-format=json`).

## Non-Goals

- Workspace-wide semantic rename. `rust_lsp_rename` already covers that via
  rust-analyzer; this design does not duplicate it.
- Operations whose soundness requires the compiler's type inference unless
  they go through the rust-analyzer surface or are validated by the
  compile-fix round.
- Macro expansion. The planner remains macro-blind; `cargo check` is the
  post-apply authority for any plan kind touching code under attribute-style
  proc-macros.
- Generative code synthesis. This is mechanical extraction with structured
  reports, not architectural judgement.
- Catch-all "fix the call sites" rewrites that depend on knowing the bound
  type of an expression. Any rewrite of `self.field` / `instance.method` /
  `Type::path` that depends on inferred types belongs in `rust_compile_fix_round`
  or in a rust-analyzer-backed kind, not in a tree-sitter plan.

## Boundary — three semantic tiers

Every plan kind below declares one of three `semantic_status` values. This
is load-bearing: it tells the caller (and the atomic-agent contract) what
authority the plan claims.

- **`syntax_only`** — tree-sitter shape only. Plan asserts byte ranges,
  node kinds, and structural validity. Does not resolve identifier
  bindings.
- **`indexed_hints`** — `syntax_only` plus best-effort lookup through a
  project-local syntactic type index. Reports may include false negatives
  (calls into std / external crates that don't resolve in the index are
  dropped, not flagged) and false positives (a project-local name that
  happens to shadow a real binding). Useful as scaffolding for human or
  agent review; never as the basis for an unguarded mechanical rewrite.
- **`lsp_verified`** — rust-analyzer signed the operation (rename,
  organize-imports, the new `rust_ra_move_item_to_module`). Semantic
  correctness deferred to the language server.

Codex round-1 critique stands: previous drafts conflated `indexed_hints`
with `lsp_verified`. The tiers are now explicit. The tree-sitter path
produces **hints**; the LSP path produces **claims**.

## Capability Surface — plan kinds

### 1. `extract_rust_impl_methods` with `deep_analysis: true`

Extends the existing plan kind. With `deep_analysis: false` (default),
behavior is unchanged. With `deep_analysis: true`, the response gains the
fields below. `semantic_status` is `indexed_hints` unless a separate
rust-analyzer pass populates `resolved_callbacks` (see below).

- **`captured_self_fields`** — every `self.<field>` and `&self.<field>` /
  `&mut self.<field>` access in the moved bodies that resolves to a field
  declared on the host struct. Each entry carries:
  - `name`
  - `field_type` (as written in the struct decl; best-effort)
  - `access_sites` — one entry per site, with `{line, column, method,
    kind: read|write, borrow_context, context}`.
  - `borrow_context` is one of: `shared_ref` (read through `&self`),
    `unique_ref` (write through `&mut self` or call on an `&mut`
    receiver), `move` (value move), `copy` (only when the type matches
    the **closed syntactic `Copy` whitelist** below), `unknown_copy`
    (type didn't match the whitelist; operator must verify),
    `interior_mutation_call` (call on a `Cell` / `RefCell` / atomic /
    `Mutex` / `RwLock`-typed field — a method-call pattern the planner
    detects syntactically when the field type was declared with one
    of those identifiers).
  - **Closed syntactic `Copy` whitelist** (recursive): a type is
    classified `copy` when its syntax is one of:
    - Rust integer/float/bool/char primitives (`i8`..`u128`, `f32`,
      `f64`, `bool`, `char`, `usize`, `isize`).
    - `&T` (shared reference, any T).
    - `*const T` / `*mut T` (raw pointers).
    - `fn(...)` / `fn(...) -> R` (function pointers). NOT function
      items (those have unnameable per-fn types) and NOT closures.
    - `()` and tuples of recursively-whitelisted types.
    - `[T; N]` arrays where T is recursively in the whitelist.
    - `Option<T>` where T is recursively in the whitelist —
      **prelude-assumed**. The classifier treats bare `Option` as
      `core::option::Option`. A local `type Option<T> = …;` shadowing
      would invalidate the classification, but tree-sitter cannot
      detect that without binding resolution. Fully-qualified
      `std::option::Option<T>` and `core::option::Option<T>` are
      binding-free and always classified.
    - User-defined types (even named `MyCopy`, even with
      `#[derive(Copy)]` visible elsewhere) classify as `unknown_copy`.
      Tree-sitter cannot verify derive macro expansion or the actual
      trait impl.
  - The planner does **not** infer a per-field mutability summary.
    Rust mutability is receiver/operation-driven, not declaration-
    level; field-level summaries leak that abstraction. Operators read
    the access-site list directly.

- **`unresolved_callbacks`** — calls inside the moved set whose receiver
  is `self`, `Self`, or unqualified, and that match the syntactic shape
  of a method call. Each entry carries `method`, `call_sites`. No
  attempt is made to resolve which `impl` / trait / blanket impl /
  deref target the call dispatches through — that is rust-analyzer's
  job. `semantic_status: indexed_hints` when the project-local syntactic
  index can show a same-file or sibling-module inherent method by that
  name; otherwise the call falls into this list.

- **`resolved_callbacks`** — only populated when a companion rust-analyzer
  pass is invoked (via `rust_ra_classify_callbacks`, a sibling plan kind
  below). Each entry carries `method`, `declaring_item`, `declaring_kind`
  (`inherent` | `trait_impl` | `blanket_impl` | `deref_target` |
  `external`), and `call_sites`. `semantic_status: lsp_verified`.

- **`inherited_generics`** and **`inherited_bounds`** — type parameters
  and `where`-clause bounds on the host impl block referenced by moved
  bodies. When the target impl block doesn't already declare them, the
  plan reports them as additions the target needs. The planner does
  not auto-inject them — that's a follow-up step via
  `rewrite_rust_item_visibility`-style or hand edit. Codex round-1 hit:
  re-elision and lifetime propagation are unsafe to auto-rewrite.

- **`captured_lifetimes`** — explicit lifetime parameters referenced
  by moved bodies that come from the host impl. Reported, not rewritten.

The fields below were proposed in rev 1 and **dropped**:

- `send_sync_hazards`. Tree-sitter cannot prove a `Send` failure. The
  failure surfaces post-apply as a `cargo check` error and is handled
  by `rust_compile_fix_round`.
- The simple field-level `source_mutable: bool`. See above.

#### FIXME marker grammar

When `deep_analysis: true` AND `status: blocked` (plan-only), the
planner scaffolds **plan-only markers** in the would-be-generated target
text:

```rust
// FIXME(refactor-plan-only): captured field `state` — read in moved
//   method `do_thing` at line 14. resolutions: extract `state` into a
//   state struct, add accessor on host, pass through a constructor
//   parameter, or promote to free function.
fn do_thing(&self) {
    let s = self.state.clone();
    ...
}
```

These markers exist in the saved plan JSON's target text only. **They
never reach the on-disk file.** Apply is skipped when the plan is in
`blocked` state.

When `deep_analysis: true` AND a separate `emit_applied_markers: true`
flag is set, the planner may insert **applied warning markers** above
code that compiles cleanly but flags a stale-capture or borrow-promotion
risk:

```rust
// FIXME(refactor-warning): borrow promotion — this delegate access
//   now goes through `&mut self.delegate` even though the original
//   read was through `&self.field`. cross-check no concurrent borrow.
self.delegate.set_counter(self.delegate.counter() + 1);
```

The two-prefix grammar is stable and greppable:

- `// FIXME(refactor-plan-only):` — never on disk.
- `// FIXME(refactor-warning):` — only above compiling code, only when
  `emit_applied_markers: true`.

**Default preference is structured plan diagnostics.** The fields above
(`captured_self_fields`, `unresolved_callbacks`, etc.) are the
authoritative record. Source markers are a review convenience. An atomic
agent that wants programmatic access should consume the JSON; an operator
reading a saved plan_path benefits from the inline markers.

### 2. `move_rust_struct_fields`

Move named fields from one struct to another (commonly a freshly extracted
"state" struct in a child module). Companion to `extract_rust_impl_methods`
for completing a `BlackboxServer` → `ServerState` split.
`semantic_status: indexed_hints`.

- **Inputs**: `source` (file containing source struct), `target` (file
  containing target struct, may be same file), `impl_name` /
  `module_name` to disambiguate when multiple structs exist, `item_names`
  (field names to move), `visibility` for moved fields on the target
  (defaults to source visibility).
- **Output**: standard `FileEdit`s plus `remaining_source_accessors`
  shaped like Java's `move_java_field` deep_analysis output:
  - For each moved field, every read/write that still lives in the
    source after the field declaration is removed.
  - Each access: `{line, column, kind: read | write |
    pattern_destructure | spread, context}`.
  - Empty `accesses` means clean move.
- **Refusal rules**:
  - `..rest` spread expressions are flagged as `kind: spread`; the
    planner does not auto-rewrite (the right rewrite depends on
    delegate strategy).
  - Pattern destructure sites are flagged as `kind: pattern_destructure`;
    same reason.
  - When the source struct has `#[repr(C)]`, `#[repr(packed)]`, or any
    non-default repr attribute, the planner emits a warning and refuses
    unless `acknowledge_repr: true` is passed.
- **Generics propagation**: `inherited_generics` (same shape as item
  extraction) reports type parameters / bounds the target struct needs
  but doesn't declare. The plan does not auto-inject.

### 3. `add_rust_delegate_field` and `update_rust_callers`

Pair plan kinds for completing a state-extract refactor.

`add_rust_delegate_field` adds `<vis> <name>: <Target>` to a source struct
and wires its construction into a designated constructor (`fn new` by
default, configurable via `impl_name` + `item_names`). When no constructor
exists, the plan refuses. `semantic_status: syntax_only`.

`update_rust_callers` rewrites source-side accesses to moved fields/methods
through the delegate. **Conservative by design.** A site is rewritten only
when the planner can prove (by syntax) that the rewrite is safe:

| Access pattern                                              | Rewrite                                  | Condition                                       |
|-------------------------------------------------------------|------------------------------------------|-------------------------------------------------|
| `self.field` in rvalue position, field type in `Copy` whitelist | `self.delegate.field()`              | field type matches the closed syntactic whitelist defined in `extract_rust_impl_methods` §1 (primitives, `&T`, raw pointers, fn pointers, tuples of whitelisted, `Option<T>` recursive). User-defined types are NOT in the whitelist. |
| `self.method(args)` where method is in moved set            | `self.delegate.method(args)`             | always (method call dispatch is unambiguous)    |
| `self.method` method-reference syntax                       | `self.delegate.method`                   | always                                          |

Every other access pattern goes into `unrewriteable_accessors` and the
operator handles it. Specifically the planner refuses to rewrite:

- Field writes (`self.field = v`, compound `self.field += v`,
  increment / decrement).
- Field reads when the field type is not provably `Copy` by syntax.
- `match` arm patterns destructuring through the moved field.
- LHS-position field references in any context.
- `..self` spread expressions.
- `mem::take(&mut self.field)`, `mem::replace`, `mem::swap` involving the
  moved field.

The rationale (Codex round-1): `self.field` → `self.delegate.field()` is a
semantic change unless `Copy` and rvalue context are both guaranteed.
Pessimistic refusal is the only sound option without type inference.
For the refused sites, the operator (or a follow-up `rust_compile_fix_round`
after a partial apply) handles the rewrite.

`semantic_status: indexed_hints`.

### 4. `extract_rust_trait`

Lift a subset of methods on a struct's inherent impl into a new `trait`,
add `impl Trait for Struct`. The Rust analog of `extract_java_interface`.
`semantic_status: indexed_hints` for v1 (rust-analyzer can verify object
safety properly; tree-sitter approximates).

- **Inputs**: `source`, `target` (new file for trait), `module_name`
  (trait name), `impl_name` (source impl header), `item_names`.
- **Behavior**:
  - Generates trait declaration with method signatures.
  - Generates `impl <Name> for <Struct>` wrapping original bodies.
  - The original inherent impl loses those methods.
  - Methods taking `Self` by value or returning `Self` get `: Sized`
    on the trait; `dyn_compatible: false` reported.
- **Reports**:
  - `object_safety_report` — structural check (no generic methods, no
    `Self` by value, no associated constants). Approximation.
  - `call_site_warnings` — under `indexed_hints`, the planner lists
    sites where `Struct::method(...)` UFCS calls or `<Struct as Trait>::method`
    qualified paths reference the lifted methods. These call sites may
    need rewrites (the trait now owns the method) but the planner does
    not auto-rewrite — they go to a follow-up `rust_compile_fix_round`.
  - `trait_in_scope_required` — module paths where callers now need
    `use <trait_path>::<TraitName>;` to invoke the lifted methods via
    method-call syntax. Listed, not rewritten.
- **Refusal**: methods whose bodies call other inherent (non-trait)
  methods on `self` not in `item_names` and not public on the host →
  refuse. Lift would orphan the calls.

### 5. `migrate_rust_type_usages`

Replace one type at type-use positions. Restructured (Codex round-1 / 2)
to accept a `replacement_kind` enum rather than a single `new_text`
string, so the planner can classify per-site legality.

- **Inputs**:
  - `source`, `module_name` (old type name as written),
    `replacement_kind` ∈ {`bare_concrete`, `box_dyn`, `arc_dyn`,
    `rc_dyn`, `impl_trait`, `generic_param_T_bounded_Trait`},
    `new_text` (the replacement expression, e.g. `Service`,
    `Arc<dyn Service>`, `impl Service`).
- **Per-`replacement_kind` legality**:
  - `bare_concrete` — legal at every type-use position.
  - `box_dyn` / `arc_dyn` / `rc_dyn` — legal at fields, params, returns,
    type aliases, generic args where the wrapper is permitted.
  - `impl_trait` — legal at fn param positions and fn return positions
    only. Refused at struct field, type alias, local binding.
  - `generic_param_T_bounded_Trait` — requires editing the enclosing
    item's generics and `where`-clause. The plan reports the edit
    set per enclosing item but does NOT auto-apply when the rewrite
    would conflict with an existing `<T>` in scope.
- **Skips** (reported under `migration_skipped`):
  - `<old>::new(…)`, `<old>::CONST`, `<old>::method` (constructor /
    associated-item paths).
  - Turbofish `::<<old>>`, `<<old> as Trait>::method`.
  - `TypeId::of::<<old>>()` reflection.
  - Pattern positions.
- **Semantic caveat**: bare identifier matching means name shadowing
  (a local `type Foo = …;` re-binding) is undetected by tree-sitter.
  `semantic_status: indexed_hints` only. For workspace-safe migration,
  rust-analyzer rename is the better surface; this plan kind is for
  scoped local migrations where the operator has confirmed no shadowing.

### 6. `rewrite_rust_error_type`

**Narrowed** from rev 1. Codex round-1 hit: `?` `From` conversion insertion
requires resolving which `From` impls exist on the new error type, which
tree-sitter cannot do. The plan kind now does only signature rewrites and
literal-form construction rewrites with explicit mapping.

- **Inputs**: `source`, `item_names` (function names), `old_text` (old
  error type), `new_text` (new error type), `error_mapping:
  {old_construction_form: new_construction_form}`.
- **Behavior**:
  - Rewrites `-> Result<T, OldErr>` to `-> Result<T, NewErr>` for the
    named functions.
  - Rewrites construction sites whose textual form matches the
    `error_mapping` keys (e.g., `bail!(OldErr::IoFail)` → `bail!(NewErr::Io)`)
    with strict string-match.
  - Reports every `?` site in the body under `question_mark_sites`,
    each classified as:
    - `text_compatible` — best-effort syntactic check that the
      converted-from type's identifier appears in a `From<X> for NewErr`
      impl in the same file or a sibling module. Hint only.
    - `unknown` — couldn't make any claim.
  - Does **not** insert `.map_err(…)` or any other conversion at `?`
    sites. The operator (or `rust_compile_fix_round` after apply)
    handles incompatibilities.
- **Refusal**: any `downcast` / `downcast_ref` on the error type in the
  named functions' bodies → refuse.

`semantic_status: indexed_hints`.

### 7. `lift_rust_inherent_to_free`

Move methods whose bodies don't read `self` into free functions in a
child module, then rewrite call sites.

- **Inputs**: `source`, `target`, `impl_name`, `item_names`.
- **Behavior**:
  - Verifies bodies have zero `self.<x>` / `Self::<x>` references
    (other than `Self` in type positions, rewritten to the concrete
    type on the target).
  - Generates `pub(crate) fn <name>(<args-minus-self>) -> <ret> { … }`.
  - Rewrites call sites: `instance.method(args)` → `module::method(args)`,
    `Struct::method(args)` → `module::method(args)`.
- **Refusal**: any `self` reference refuses the lift for that method.
- **Caveat**: lifetime elision rules differ between methods and free
  functions. The plan preserves explicit lifetimes verbatim and never
  re-elides (Codex round-1 — re-elision is unsafe without type info).

`semantic_status: indexed_hints`.

### 8. `rust_ra_move_item_to_module` *(new — rust-analyzer-backed)*

The clean primitive for splitting `main.rs` and similar god files when
the operator wants find-references + import edits handled correctly.
`semantic_status: lsp_verified`.

- **Inputs**: `source`, `target`, `item_names`, `item_kinds`.
- **Scope (v1)**: top-level items only — free functions, types,
  consts/statics, modules (where RA exposes a code-action for them).
- **NOT in scope (v1)**: impl methods. Impl-method extraction has
  receiver/inherent-vs-trait/attribute/`#[tool_router]`/visibility
  quirks that rust-analyzer's move-to-module code action does not
  handle uniformly. `extract_rust_impl_methods` remains the
  syntax-primary surface; if a future rust-analyzer release exposes
  a precise extract-impl-method-to-module code action, add a
  separate `rust_ra_move_impl_method_to_module` kind. Do not overload
  the item-move kind.
- **Implementation note**: routes through the warm `LspSessionManager`
  used by `rust_lsp_rename` and `rust_organize_imports`. Cold-start
  cost is paid once per `(project_root, Rust)` pair.

### 9. `rust_ra_classify_callbacks` *(new — rust-analyzer-backed)*

Resolves the `unresolved_callbacks` produced by `extract_rust_impl_methods`'s
`deep_analysis` pass. Run as a follow-up plan in the same
`bbox_refactor_run` when the operator wants `resolved_callbacks` populated.

- **Inputs**: `source`, `item_names` (method names whose bodies should be
  analyzed), and either the plan ref from a prior `extract_rust_impl_methods`
  step or fresh discovery.
- **Behavior**: invokes rust-analyzer `textDocument/references` plus
  `textDocument/definition` for each call site to determine the declaring
  item, classifying as `inherent` | `trait_impl` | `blanket_impl` |
  `deref_target` | `external`.
- **Output**: populates `resolved_callbacks` on the plan response.

`semantic_status: lsp_verified`.

### 10. `rust_compile_fix_round` *(new — load-bearing, paired with runner extension)*

The repair primitive Codex round-1 identified as missing. **Requires
two changes that must land together:**

#### Runner extension

`bbox_refactor_run` command steps gain two fields:

- `capture: "rustc_json"` — the runner parses `cargo` output as JSON
  diagnostic messages (`--message-format=json` is the canonical command
  form) and stores the resulting diagnostic set under a named ref
  (defaulting to `"last"`).
- `on_failure: "continue_for_repair"` — a third failure mode beyond
  today's required/optional. The runner does NOT roll back on this
  step's failure; instead it passes the captured diagnostics to the
  next plan step, which must consume them.

Today's `RefactorRunStep::Command` shape lives at `src/refactor/mod.rs:420`;
the rollback path at `:1343`; command output is currently a truncated
string at `:1424` (Codex round-3 grep). The runner extension is additive.

#### Planner extension — `rust_compile_fix_round`

A new plan kind that consumes a diagnostic ref and produces a reviewable
`RefactorPlan`.

- **Inputs**: `diagnostics_ref` (default `"last"`), `project_dir`,
  optional `restrict_to_files` filter.
- **Behavior**: classifies rustc/rust-analyzer-style diagnostics and
  generates plan steps:
  - `unresolved import` → `add_rust_use_decl` plans for resolvable
    paths.
  - `function/method is private` → `rewrite_rust_item_visibility`
    proposals to `pub(crate)` (operator reviews; not auto-applied).
  - `trait ... is not in scope` → `add_rust_use_decl` for the trait.
  - `no method named X on Y` (after move) → `add_rust_use_decl` for
    the trait owning X, or flag as moved-method call site needing
    delegate rewrite.
  - `cannot move out of borrowed content` / `borrow checker` errors
    → not auto-repaired; flagged as `leftovers` with the original
    diagnostic preserved.
  - `the trait bound ... is not satisfied` → flagged; not auto-repaired.
- **Output**: a standard `RefactorPlan` (reviewable, hash-checked,
  applyable independently). If no safe edits can be generated, the
  plan is empty and the run rolls back via the repair transaction
  invariant below.

#### Repair transaction invariant

> A soft-failed (`on_failure: continue_for_repair`) command may continue
> only if a later repair step consumes its diagnostics. Snapshots from
> the soft-failed step stay live **until the run reaches terminal
> success** (every subsequent required step also succeeds). If ANY
> later step fails — repair plan, validation command, or final test
> command — the ENTIRE run rolls back atomically, including the
> soft-failed command's prior state.

Codex round-3/4/5 spec. This prevents `continue_for_repair` from
becoming a quieter `required: false`. Specifically: it's not enough
for the next required step to succeed; the run must run to terminal
success for the soft-failed snapshot to be released. A late test
failure rolls back everything from the soft-failed point forward.

**Multi-repair composition.** Runs may contain multiple sequential
soft-failed steps (e.g., post-extract `cargo check` repair, then
post-compile-fix `cargo check` repair). Each opens a live repair
obligation; terminal success releases all such obligations only after
every captured diagnostic is consumed (or explicitly left over via
`rust_compile_fix_round`'s `leftovers`) AND every later required
validation step succeeds. Any uncovered diagnostic at terminal time
fails the run and rolls back every soft-failed step's snapshot.

Canonical composition:

```text
{"op":"plan","kind":"extract_rust_impl_methods", ...}
{"op":"plan","kind":"add_rust_mod_decl", ...}
{"op":"command","command":"cargo","args":["check","--message-format=json"],
 "capture":"rustc_json","on_failure":"continue_for_repair"}
{"op":"plan","kind":"rust_compile_fix_round","diagnostics_ref":"last"}
{"op":"command","command":"cargo","args":["check"],"required":true}
{"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
```

`semantic_status: lsp_verified` (consumes compiler authority).

### 11. `rust_impl_partition_analysis` *(new — first-class analysis)*

The graph-only primitive promoted from rev 1's hand-waved
"rust-impl-partition-suggester" atom. Codex round-2 distinction: this
tool produces the graph; clustering is a SEPARATE atom that may accept
the graph and run any algorithm on it.

- **Inputs**: `source`, `impl_name`.
- **Output**:
  ```json
  {
    "methods": [
      {
        "name": "build_exec_args",
        "reads": [],
        "writes": [],
        "calls": ["transient_blackbox_url", "claude_mcp_config_json"],
        "unresolved_callbacks": [],
        "attrs": ["#[tool]"],
        "router": "search_tools"
      }
    ],
    "fields": [
      { "name": "store_dir", "type": "PathBuf", "shared_by": ["a","b","c"] }
    ],
    "edges": [
      { "from": "method:a", "to": "method:b", "kind": "calls" },
      { "from": "method:a", "to": "field:store_dir", "kind": "reads" }
    ]
  }
  ```
- **Use**: humans or atoms read the graph and propose partitions. An atom
  may run a clustering algorithm (modularity, label propagation, attribute
  affinity for `#[tool_router]` groups), but that's downstream of this
  primitive.

`semantic_status: indexed_hints`.

### 12. `rust_match_arm_to_strategy` *(new — provider catalog shape)*

Specialized for the providers.rs shape Codex round-2 identified. The
plan kind generates a **hybrid** restructuring, not pure trait or pure
table.

The shape it produces:

- `ProviderSpec` (data) — static facts: capabilities, env-bin names,
  model catalogs, effort catalogs. Codex pointed at `providers.rs:69, 179`
  for the data clusters in the current code.
- `ProviderDriver` modules (behavior) — argv construction, MCP CRUD,
  filter translation, event parsing, session discovery. Codex pointed
  at `providers.rs:279, 600, 806, 1094` for the behavior families.
- Driver-family sharing — `OpenCode | Glm | Deepseek | Inception` share
  a driver with different model specs.

- **Inputs**: `source`, `enum_name` (the enum whose variants drive the
  match), `behavior_family_names` (the match-on-enum methods to lift
  into drivers), `data_field_names` (the simple-getter methods to lift
  into specs), `driver_share_groups: [["Glm","Deepseek","Inception"]]`.
- **Output**: per-variant module file with `Spec` constants + `Driver`
  trait impl, plus a router function on the enum that dispatches by
  variant.
- **Refusal**: enum variants with non-trivial associated data (variants
  carrying data beyond a `String` name) refuse — the rewrite needs
  judgment about whether the data goes in spec, driver, or stays on
  the variant.

`semantic_status: indexed_hints`. Pairs with `rust_compile_fix_round`
for the call-site cleanup.

### 13. `rust_public_api_guard` *(new — analysis only)*

Precondition analysis for any plan touching `pub` items. Runs as a
plan kind whose output is purely advisory; it never applies edits.

- **Inputs**: `source` (file or directory), `proposed_changes` (the
  refs of plan steps that follow).
- **Output**:
  - `public_items_touched: [{path, kind, name}]` — `pub` items whose
    declarations or signatures are affected.
  - `public_api_delta_summary` — additions / removals / signature
    changes. Best-effort textual comparison.
  - `crate_root_re_exports_affected: [{path, name}]` — `pub use` items
    in `lib.rs` / `main.rs` whose targets are in the touched set.
- **Use**: atomic agents that modify potentially-public surfaces
  (notably `inline_rust_re_export` mutation, deferred below) MUST run
  this and `bbox_note(kind="blocked")` if the delta is non-empty,
  unless the operator explicitly passed `acknowledge_public_api_change: true`.

`semantic_status: indexed_hints`.

## Plan kinds explicitly cut from rev 1

- **`move_rust_const_items`** — cut. `extract_rust_items` already handles
  `item_kinds=["const_item", "static_item"]`. The `keep_copy` mode does
  not need a const-specific plan kind; if retained-copy semantics become
  common across item kinds, a future generic `copy_rust_items` is the
  right shape. Visibility-widening-on-retention is the operator's
  explicit follow-up: `extract_rust_items` + `rewrite_rust_item_visibility`,
  two reviewable steps.

- **`send_sync_hazards`** — cut. Tree-sitter cannot prove. Compiler
  diagnostics via `rust_compile_fix_round` carry it.

- **`dejunk_rust_struct` (derive-collapse)** — cut from this doc. It's a
  legitimate Rust modernization plan kind but it's not part of closing
  the Java deep-analysis extraction gap. Moves to a separate
  `design/refactor-rust-derive-collapse.md` (to be authored).

- **`inline_rust_re_export` mutation** — cut. Replaced by an analysis-only
  pass surfaced through `rust_public_api_guard`'s
  `crate_root_re_exports_affected` field. The mutation form is
  deferred until the public-API guard surface is solid and
  cargo-semver-style tooling is integrated.

- **`rewrite_rust_error_type` automatic `?` conversion insertion** —
  narrowed. The kind remains, but `?` site insertion is removed
  (Codex round-1: needs type-checker). The kind now does signatures
  + explicit-mapping construction rewrites only. Conversion gaps
  surface through `rust_compile_fix_round`.

## Cross-cutting design choices

### `deep_analysis` defaults

Plan-level default: **OFF**. Matches Java's cost precedent
(`sm-refactor-java` §3). The walk crosses files through the project
type index; on small moves it's negligible, on large clusters it
dominates plan time.

Atom-level requirement: refactor atoms (`design/refactor-agents.md`)
**REQUIRE** `deep_analysis: true` via their `inputs.schema`. Direct
human callers running cheap structural moves can opt out.

This resolves the Codex round-1 push for "default ON" — the silent-
miscompile risk it identified is real, but the right fix is the atom
contract, not the plan default. Atoms are the supervised surface;
direct plan-kind callers are operators who know what they're doing.

### Plan output size and `output_path`

Like the Java side (Gap 3 + response-size), large Rust plans support
`output_path`: write full `RefactorPlan` JSON to disk and return a
compact `RefactorPlanSummary` for the MCP response. Apply via
`bbox_refactor_apply(plan_path="…", confirm=true)`.

Codex round-1/4 questioned arbitrary `output_path`. Resolution: the
parameter accepts a relative path that resolves under a daemon-owned
plan directory (`$BLACKBOX_STATE_DIR/refactor/plans/`), not arbitrary
filesystem locations. Absolute paths are rejected. The sibling-friendly
slot leaves room for future `$BLACKBOX_STATE_DIR/refactor/diagnostics/`
and `$BLACKBOX_STATE_DIR/refactor/runs/` directories without renames.

**Paired apply invariant**: `bbox_refactor_apply(plan_path=…)` reads
must be restricted to the same daemon-owned slot. The apply path
rejects `plan_path` values outside `$BLACKBOX_STATE_DIR/refactor/plans/`
to prevent operators from pointing apply at hand-crafted JSON in
arbitrary locations.

### `semantic_status` taxonomy

Three tiers, declared per plan kind:

- `syntax_only` — tree-sitter structural validity only.
- `indexed_hints` — adds project-local syntactic index lookups;
  reports may include false negatives (std/external calls dropped)
  and false positives (shadowed names). Hints, not claims.
- `lsp_verified` — rust-analyzer signed the operation.

Codex round-1 hit: the rev-1 doc named the middle tier `project_indexed`,
which overclaimed authority. `indexed_hints` is the corrected name.

### FIXME grammar default

Structured plan diagnostics (the `captured_self_fields`,
`unresolved_callbacks`, etc. fields) are the authoritative record.
FIXME source markers are a review convenience for operators reading
saved plan files. Atomic agents consume the JSON; humans consume the
markers. Both reach the same conclusion.

`// FIXME(refactor-plan-only):` never reaches on-disk source.
`// FIXME(refactor-warning):` only above compiling code, only when
`emit_applied_markers: true`.

### Composition with `bbox_refactor_run`

The canonical god-impl split with compile-fix becomes:

```text
{"op":"plan","kind":"extract_rust_impl_methods","deep_analysis":true, ...}
{"op":"plan","kind":"rust_ra_classify_callbacks", ...}              // optional, populates resolved_callbacks
{"op":"plan","kind":"add_rust_router_to_sum", ...}
{"op":"plan","kind":"add_rust_mod_decl", ...}
{"op":"plan","kind":"rewrite_rust_item_visibility", ...}
{"op":"plan","kind":"rust_organize_imports", ...}
{"op":"command","command":"cargo","args":["check","--message-format=json"],
 "capture":"rustc_json","on_failure":"continue_for_repair"}
{"op":"plan","kind":"rust_compile_fix_round","diagnostics_ref":"last"}
{"op":"command","command":"cargo","args":["check"],"required":true}
{"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
```

The state-extract sequence:

```text
{"op":"plan","kind":"extract_rust_items","item_kinds":["struct_item"], ...}
{"op":"plan","kind":"move_rust_struct_fields","deep_analysis":true, ...}
{"op":"plan","kind":"add_rust_delegate_field", ...}
{"op":"plan","kind":"update_rust_callers", ...}                     // conservative; reports unrewriteable_accessors
{"op":"command","command":"cargo","args":["check","--message-format=json"],
 "capture":"rustc_json","on_failure":"continue_for_repair"}
{"op":"plan","kind":"rust_compile_fix_round","diagnostics_ref":"last"}
{"op":"command","command":"cargo","args":["check"],"required":true}
```

Cfg matrix validation is just multiple command steps, not a new
primitive:

```text
{"op":"command","label":"check-default","command":"cargo","args":["check"]}
{"op":"command","label":"check-all-features","command":"cargo","args":["check","--all-features"]}
{"op":"command","label":"check-no-default","command":"cargo","args":["check","--no-default-features"]}
```

### Belongs in the agent layer, not the plan-kind layer

The following were proposed in earlier drafts as plan kinds but belong
upstream:

- `rust_parser_dialect_split` — domain-specific to `src/parser.rs`.
  Built from `extract_rust_items` + `move_rust_struct_fields` in an
  atom.
- `rust_unit_test_shard` — the `rust-test-island-extract` atom
  (`design/refactor-agents.md` catalog). Built from `extract_rust_items`.
- "Suggest the partition" — `rust_impl_partition_analysis` produces
  the graph; a clustering atom does the suggestion.

## Safety Rules (additive)

In addition to existing Rust refactor safety rules (`sm-refactor-rust`):

- Reports under `semantic_status: indexed_hints` are best-effort.
  Resolution failures default to "report nothing" rather than
  "report a guess." `cargo check` remains the post-apply authority.
- `move_rust_struct_fields` refuses on non-default `#[repr(...)]`
  unless `acknowledge_repr: true`.
- `extract_rust_trait` object-safety reporting under `indexed_hints`
  is structural; rust-analyzer is the authority for object safety.
- `rewrite_rust_error_type` cannot detect downstream callers depending
  on the old error's API (variant patterns, `Display` output). The
  plan touches signatures + matched-construction sites; broader audit
  is the operator's job.
- `update_rust_callers` refuses any rewrite outside the narrow safe
  list above (rvalue `Copy` reads, unambiguous method calls). All
  other sites go to `unrewriteable_accessors`.
- `rust_compile_fix_round` never auto-resolves borrow-checker errors
  or trait-bound failures. Those are flagged as leftovers and the
  run rolls back per the repair transaction invariant.

## Cross-Surface Invariants

The fixes below are not specific to any one plan kind; they bind the
entire refactor surface and the atomic-agent contract on top of it
(`design/refactor-agents.md`).

### Plan-file slot policy

`output_path` (write) and `bbox_refactor_apply(plan_path=…)` (read)
both target only `$BLACKBOX_STATE_DIR/refactor/plans/`. Absolute paths
and paths escaping the slot are rejected. This keeps the planner and
applier symmetric: a plan written here can be applied; a plan-shaped
JSON outside this slot cannot.

### RA-backed plan kinds fail closed

`rust_lsp_rename`, `rust_organize_imports`, `rust_ra_move_item_to_module`,
and `rust_ra_classify_callbacks` require rust-analyzer. When the LSP
session is unavailable (binary missing, init timeout, crashed mid-run),
these plan kinds **fail closed**: they return an `error.lsp_unavailable`
response. They **must not** silently downgrade to a `syntax_only` /
`indexed_hints` approximation, because callers chose the LSP-backed
kind specifically for `semantic_status: lsp_verified`.

This is intentional asymmetry: `rust_organize_imports` already has a
documented tree-sitter fallback for the Java side (`sm-refactor-java`),
but the Rust LSP-backed kinds in this design treat fallback as a
silent semantic downgrade and refuse it.

### Operator-authority opt-outs

Two opt-out flags appear in this design as escape hatches:
`acknowledge_repr` (on `move_rust_struct_fields`) and
`acknowledge_public_api_change` (on `rust-error-migrate` and any
future kind that touches public surfaces).

**These flags are operator authority, not agent discretion.** Atomic
agents:

- MAY pass these flags through from their `inputs` to the underlying
  plan kind when an operator explicitly set them.
- MUST NOT default them.
- MUST NOT infer them from context ("the public-API delta looks
  small" is not a valid reason to set `acknowledge_public_api_change`).
- MUST NOT silently set them after seeing a refusal.

An atom that sets these flags on the operator's behalf is no longer
an atomic agent; it's a general executor with discretion.

### `bbox_refactor_run` command step allowlist for atoms

The runner today accepts arbitrary `command` values in command steps.
This is necessary for general operator workflows. **For atom-dispatched
runs**, the agent contract restricts command steps to a small allowlist:

- `cargo check` (with any args; `--message-format=json` and feature
  flags allowed).
- `cargo test` (with any args).
- `cargo clippy` (with any args).
- `cargo fmt` — only when `touches` is explicitly declared.
- `cargo build` (allowed but typically wasteful).

Any other command in an atom-dispatched run is a prompt-discipline
violation. The atom should refuse to compose it. Runner-side
enforcement of this allowlist is a v2 path that requires the runner
to know whether a run came from `bro_agent_dispatch`; v1 relies on
the prompt template and the brofile's `Bash`/`Write`/`Edit` denial
to prevent the worst escape routes.

Mutating commands not in the allowlist additionally must declare
`touches` so the runner can snapshot/rollback. An atom-dispatched run
with an undeclared-`touches` mutating command is unsupported.

## Open Design Questions

1. **Lifetime / generic propagation policy.** `extract_rust_impl_methods`
   `deep_analysis` reports `inherited_generics` and `inherited_bounds`
   but does not auto-inject them. Whether a follow-up plan kind should
   auto-propagate (with conflict detection) or whether this stays as
   operator-resolved is open. Auto-injection is convenient when no
   conflict exists; the risk is when the target impl block already
   declares `<T>` with different bounds.
2. **`rust_ra_classify_callbacks` granularity.** Whether it should
   resolve every call site in moved bodies or accept a `restrict_to`
   filter for cost control on large extractions. Currently spec'd as
   resolve-all.
3. **Repair transaction depth.** Can `rust_compile_fix_round` itself
   produce edits that need a second compile-fix round? The runner
   currently has no loop primitive; the invariant says repair-or-final-
   validation. Whether to allow N rounds with a budget or cap at one
   is open.
4. **Driver-family detection in `rust_match_arm_to_strategy`.** The
   plan kind currently accepts `driver_share_groups` as explicit input.
   An auto-detection pass (find enum variants whose match arms produce
   structurally-identical code modulo identifiers) is harder than it
   sounds because the OpenCode-family case has subtle per-variant
   differences. Leave to operator for v1.
5. **Brofile prerequisites.** Refactor atoms need a narrow
   `rust-refactor-persona` brofile because the agent filter overlay can
   only ADD denies, not narrow the brofile's allows (Codex round-1).
   Whether this brofile ships with the daemon or operators author it
   per-install is a question for `design/refactor-agents.md`.
6. **Workspace vs project-scoped type index.** `deep_analysis` consults
   a project-local syntactic index. Expanding to workspace-wide
   resolution (multi-crate) is straightforward but changes the cost
   model. Open whether the workspace mode is opt-in via a flag or
   always available when a Cargo workspace is detected.

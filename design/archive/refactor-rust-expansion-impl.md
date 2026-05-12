# Rust Refactor Expansion — implementation skeleton

Companion to `design/archive/refactor-rust-expansion.md`. Each phase names a
discrete implementation chunk: scope, realizes, components, gates,
known follow-ups. Phases are dependency-ordered. No timelines —
landing a phase unblocks dependents; landing all phases realizes the
design.

This skeleton assumes the existing refactor backend
(`src/refactor/{mod,rust,java,tests}.rs`) and the warm
`LspSessionManager` (used by `rust_analyzer_rename` at
`src/refactor/rust.rs:1939` and `rust_analyzer_organize_imports` at
`src/refactor/rust.rs:1991`) are in place. The agents-impl doc
(`design/refactor-agents-impl.md`) depends on this skeleton — atoms
specified there need RX-F1a/F1b/F2a/F2b/A1a-d/A2/C1 plus RX-V1/V2
docs at minimum before they can be authored as JSON.

Phases are prefixed `RX-` to disambiguate from `AS-` (agent system)
and other plan prefixes.

---

## Substrate phases

These shape the plan/run surface every later phase depends on. Each
ships independently; their splits per codex round-7 review reflect
unrelated blast radius.

### Phase RX-F1a — `semantic_status` taxonomy migration

**Scope.** Migrate the existing `SemanticStatus` enum to the
three-tier `SyntaxOnly` / `IndexedHints` / `LspVerified` taxonomy
described in the design doc.

**Realizes.** `design/archive/refactor-rust-expansion.md` "Boundary — three
semantic tiers".

**Components.**
- Today's enum at `src/refactor/mod.rs:474` is:
  ```rust
  pub enum SemanticStatus {
      StructuralOnly,
      LspVerified,
      Unverified,
  }
  ```
  with serialized forms `structural_only` / `lsp_verified` /
  `unverified`.
- Rename `StructuralOnly` → `SyntaxOnly` (serialized `syntax_only`).
  Add `IndexedHints` (serialized `indexed_hints`). Keep `LspVerified`.
- Deprecation path for `Unverified`:
  - Serde alias: accept `"unverified"` on deserialize, map to
    `IndexedHints`.
  - Search every emit site (today's `SemanticStatus::Unverified`
    constructions across `src/refactor/{mod,rust,java}.rs`); each
    becomes either `SyntaxOnly` or `IndexedHints` per the design
    doc's per-plan-kind classification.
  - Existing `StructuralOnly` emit sites (e.g.,
    `src/refactor/mod.rs:1490, 1561, 1592, 1641`) become
    `SyntaxOnly`.
- Update tool docstrings and `sm-refactor` / `sm-refactor-rust` /
  `sm-refactor-java` to reference the new names.

**Gates.**
- Enum value rename builds; existing tests pass after mechanical
  rename.
- Old serialized form `"unverified"` round-trips to `IndexedHints`
  via serde alias (back-compat).
- New serialized forms `"syntax_only"` / `"indexed_hints"` /
  `"lsp_verified"` accepted on deserialize and emitted on serialize.
- `sm-refactor*` entries updated as part of this landing (not
  follow-up).

**Follow-ups.**
- Eventually drop the `"unverified"` alias once external callers
  have migrated; track via a deprecation note in `sm-refactor`.

---

### Phase RX-F1b — Plan-file slot policy + paired apply read restriction

**Scope.** Lock down the daemon-owned plan-file slot at
`$BLACKBOX_STATE_DIR/refactor/plans/`. Migrate `output_path`
(planner write) and `bbox_refactor_apply(plan_path=…)` (applier
read) to resolve under the slot. This is a behavior change to
existing code (today's `output_path` resolves relative to
`project_dir` or `cwd` per `src/refactor/mod.rs:293`).

**Realizes.** `design/archive/refactor-rust-expansion.md` "Plan output size
and `output_path`"; "Cross-Surface Invariants — Plan-file slot
policy".

**Components.**
- New module `src/refactor/plan_slot.rs` resolving relative
  `output_path` strings under `$BLACKBOX_STATE_DIR/refactor/plans/`.
  Rejects absolute paths and paths escaping the slot (canonicalize
  + prefix check).
- `output_path` resolution at the planner-write call site (around
  `src/refactor/mod.rs:293` docstring; the actual resolution lives
  with the write helper) routes through the slot module.
- `bbox_refactor_apply(plan_path=…)` and any `op:"apply"`-style read
  paths route through the same slot module.
- Migration: existing callers passing relative `output_path` values
  whose resolution previously landed under `project_dir` now land
  under the slot. This is a breaking change for callers that relied
  on the old resolution. Tradeoff acknowledged in the design doc;
  callers update or pass through the slot.
- Sibling slot reservation: `$BLACKBOX_STATE_DIR/refactor/diagnostics/`
  and `.../runs/` are reserved (rejected from `output_path` writes
  until later phases populate them).
- `sm-refactor` updated to document the new slot policy.

**Gates.**
- Round-trip: `output_path="my-plan.json"` writes to
  `<state-dir>/refactor/plans/my-plan.json`.
- Reject `output_path="/tmp/x.json"` (absolute) with
  `error.bad_input(code=plan_path_outside_slot)`.
- Reject `output_path="../../etc/passwd"` after canonicalization.
- `bbox_refactor_apply(plan_path="../../foo.json")` rejects with the
  same code.
- Migration test: existing fixtures using project_dir-relative
  `output_path` updated; one regression test confirms the new
  resolution lands files in the slot.

**Follow-ups.**
- Cleanup policy for the slot (LRU eviction? operator-only?).
  Tracked separately.

---

### Phase RX-F2a — Runner `capture: rustc_json` plumbing

**Scope.** Additive plumbing: command steps gain an optional
`capture` field; the runner parses captured stdout as cargo-message
JSON and stashes the result in a per-run scratch struct. No
transaction-semantics change in this phase.

**Realizes.** `design/archive/refactor-rust-expansion.md` §10 "Runner
extension" subsection (the capture half).

**Components.**
- Add optional `capture: Option<CaptureSpec>` to the `Command`
  variant of `RefactorRunStep` at `src/refactor/mod.rs:406`.
  `CaptureSpec` enum starts with one variant: `RustcJson`.
- Run-context scratch struct keyed by named ref (default `"last"`),
  storing parsed diagnostics from each `capture`-enabled step.
- Diagnostic parser: tolerates malformed lines (non-JSON chatter,
  partial messages), collects `compiler-message`-shaped entries
  with their `level`, `code`, `message`, `spans`, and `children`
  (suggestions). Other message kinds dropped.
- `RefactorRunStepReport` at `src/refactor/mod.rs:597` gains
  optional `captured_diagnostics_summary: {count, severity_counts:
  {error: N, warning: M, ...}}`. Full diagnostic body NOT in the
  report (size budget) — only summary.
- `sm-refactor` updated to mention the new field.

**Gates.**
- Run with `[command(capture=rustc_json) on cargo check]` on
  passing code: report carries `captured_diagnostics_summary` with
  warning count, no errors.
- Same on failing code: report carries error count; this phase's
  semantics are unchanged from today (required=true still rolls
  back, required=false still continues).
- Malformed cargo output (e.g., panic mid-build) does not crash the
  runner; captured set is best-effort.
- Existing required/optional command-step tests pass (regression).

**Follow-ups.**
- Repair obligations and rollback cursor (RX-F2b).
- Additional capture formats (clippy JSON, miri) when later phases
  need them.

---

### Phase RX-F2b — `continue_for_repair` + repair obligations + rollback cursor

**Scope.** Transaction-semantics change: third failure mode on
command steps; obligations tracked; multi-soft-fail composition
with terminal-success commit policy.

**Realizes.** `design/archive/refactor-rust-expansion.md` §10 "Repair
transaction invariant"; "Multi-repair composition".

**Components.**
- Add optional `on_failure: Option<OnFailure>` to the `Command`
  variant. Variants: `Required` (default; today's semantics),
  `Optional` (= today's `required: false`), `ContinueForRepair`
  (new). Codex round-7 noted that `required: bool` exists today on
  the Command variant; the new field supersedes it. Migration:
  - When `on_failure` is unset AND legacy `required: true`, treat as
    `Required`.
  - When `on_failure` is unset AND legacy `required: false`, treat
    as `Optional`.
  - When `on_failure` is set, ignore legacy `required` (or error if
    both contradict).
- Run-context scratch extended with `obligations: Vec<Obligation>`
  where `Obligation { ref_name, opened_at_step_idx, captured_count,
  status: Open | Consumed | LeftOver }`.
- Soft-fail handling at the runner's command-step branch (today's
  rollback path at `src/refactor/mod.rs:1343` is the symmetric
  point):
  - On exit_code != 0 with `ContinueForRepair`: open an Obligation
    pointing at the named diagnostics ref. Do NOT roll back.
- Plan-step branch records consumption: `rust_compile_fix_round`
  (RX-C1) explicitly marks the obligation `Consumed` after its
  classifier ran; if the classifier produced `leftovers` and the
  operator/atom acknowledged them, the obligation status becomes
  `LeftOver` (still acceptable for commit).
- **`first_live_soft_fail_cursor` semantics** (Codex round-2):
  consumed/leftover obligations are still LIVE rollback anchors
  until terminal success. Marking an obligation `Consumed` does
  NOT release its cursor — only terminal success does. The
  rollback cursor is the **earliest soft-fail step whose
  obligation has not yet committed**, which means in practice
  "the first soft-fail step that has ever been opened in this
  run, until terminal success rolls everything forward to commit."
- Terminal-success policy: at run end, commit only if every
  Obligation is `Consumed` or `LeftOver` AND every `Required` step
  succeeded. Otherwise roll back ALL snapshots from the
  `first_live_soft_fail_cursor` onward (or from step 0 if any
  required-step failure occurred before any soft fail).
- Multi-soft-fail composition: each soft fail opens its own
  obligation; terminal success requires all are resolved. On
  non-terminal-success, rollback goes to the FIRST soft-fail
  cursor in the run, NOT the earliest-still-unresolved one,
  because consumed obligations did not release their cursors.
- `sm-refactor` documents the new failure mode.

**Gates.**
- `[plan, command(continue_for_repair, capture=rustc_json),
  plan(compile_fix), command(check, required=true)]`:
  - compile_fix resolves all diagnostics, final check passes →
    commits.
  - compile_fix puts items in leftovers, final check passes →
    commits with leftovers visible in report.
  - compile_fix produces no edits AND no leftovers → final fails
    because obligation unresolved → roll back all snapshots from
    the soft-fail point.
  - Final check fails → roll back everything from the soft-fail
    point.
- Multi-soft-fail composition: two `continue_for_repair` commands
  with one compile_fix between them. If only the first obligation
  resolves and the second is unconsumed, terminal commit fails and
  rolls back from the FIRST soft-fail step (because consumed
  obligations do NOT release their cursor — the first cursor
  remains live until terminal success).
- Legacy `required: true/false` callers behave identically when
  `on_failure` is unset (regression).

**Follow-ups.**
- Loop primitive: should compile_fix itself trigger another
  cargo-check + compile_fix round? Open design question; v1 is one
  round. The obligation tracking supports a future loop primitive
  without further runner changes.

---

## `extract_rust_impl_methods` deep_analysis surface

The major analysis primitive. `deep_analysis: Option<bool>` already
exists on `RefactorPlanParams` at `src/refactor/mod.rs:268` (used
today by Java extract-class kinds). The Rust phases below wire the
flag through to a Rust-specific analysis module and populate the
deep-analysis fields described in the design doc.

### Phase RX-A1a — Rust-side `deep_analysis` flag wiring

**Scope.** Wire the existing `deep_analysis: Option<bool>` flag
through `extract_rust_impl_methods`'s plan path. Without this
phase, passing `deep_analysis: true` on a Rust plan kind has no
effect.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1
"`extract_rust_impl_methods` with `deep_analysis: true`" entry
point.

**Components.**
- The Rust extract-impl-methods plan implementation (in
  `src/refactor/rust.rs`) reads `params.deep_analysis.unwrap_or(false)`
  and routes into a new `rust_deep` analysis module (created here
  as a stub; populated by RX-A1b-d).
- New module `src/refactor/rust_deep.rs` (or sub-module of `rust.rs`)
  exposing a `deep_analyze_extract(...)` entry function. Stub
  returns empty deep-analysis fields in this phase.
- `RefactorPlanSummary` extended with optional deep-analysis fields
  (`captured_self_fields`, `unresolved_callbacks`,
  `inherited_generics`, `inherited_bounds`, `captured_lifetimes`)
  serialized only when `deep_analysis: true`.
- `semantic_status: IndexedHints` for plans with deep_analysis on.

**Gates.**
- **Canonical-JSON regression**: `extract_rust_impl_methods` with
  `deep_analysis` unset OR explicitly false produces a plan whose
  canonical (sorted-key, normalized-whitespace) JSON is identical
  to the pre-RX-A1a output for a fixture set (small/medium/large
  impl blocks). New deep-analysis fields are omitted via
  `skip_serializing_if = "Option::is_none"` / `Vec::is_empty`; if
  byte-identical preservation is intentional via the omission
  scheme, an additional byte-identical assertion is welcome but
  not required (Codex round-2: byte-identical is brittle against
  pretty-print/field-order churn).
- `deep_analysis: true` produces a plan with the new field schema,
  all fields empty (no analysis implementation yet).
- `semantic_status: IndexedHints` on the deep-analysis plan.

**Follow-ups.**
- Field population in RX-A1b/c/d.

---

### Phase RX-A1b — `captured_self_fields` + borrow_context + Copy whitelist + interior mutation

**Scope.** Populate `captured_self_fields` with per-site
`borrow_context` classification. Implement the closed syntactic
`Copy` whitelist (primitives + `&T` + raw pointers + fn pointers +
`()` and tuples + `[T; N]` + `Option<T>` recursive; user-defined
types → `unknown_copy`). Detect interior-mutation calls on `Cell` /
`RefCell` / `Mutex` / `RwLock` / atomics.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1
`captured_self_fields` section + Copy whitelist.

**Components.**
- Body walk in `rust_deep` for moved-method bodies; collects
  `self.<field>` / `&self.<field>` / `&mut self.<field>` accesses
  resolving to fields on the host struct (which is in scope —
  syntactic resolution is sufficient because the host struct decl
  is visible to the planner).
- `borrow_context` classifier:
  - `shared_ref` for read through `&self`.
  - `unique_ref` for write through `&mut self` or method call on
    `&mut` receiver.
  - `move` for value moves (e.g., `let x = self.field;` of
    non-Copy field; `kind: write`-shaped operation moving out).
  - `copy` only when field type matches the Copy whitelist.
  - `unknown_copy` when the type doesn't match the whitelist.
  - `interior_mutation_call` when the field type identifier is one
    of the well-known interior-mutation types (`Cell`, `RefCell`,
    `Mutex`, `RwLock`, `Atomic*`) AND the access is a method call.
- Closed Copy whitelist as a small recursive matcher over
  tree-sitter type-expression nodes.
- Type-extraction helper: given a field name, find its declared
  type in the host struct decl (already accessible to the
  planner).

**Gates** (each named fixture is a minimal Rust snippet checked into
`src/refactor/tests.rs`):
- `fixture_borrow_shared_ref.rs` — `let n = self.count;` with
  `count: u32` → classified `copy`.
- `fixture_borrow_unique_ref.rs` — `self.cache.insert(k,v);` with
  `cache: HashMap<K,V>` → classified `unique_ref` (method on
  `&mut self.cache`).
- `fixture_borrow_move.rs` — `let s = self.string;` with `string:
  String` → classified `move` (non-Copy).
- `fixture_borrow_interior_mutation.rs` — `self.cache.borrow_mut()`
  with `cache: RefCell<HashMap<K,V>>` → classified
  `interior_mutation_call`.
- `fixture_copy_whitelist_primitive.rs` — `u32` field → `copy`.
- `fixture_copy_whitelist_ref.rs` — `&'a str` field → `copy`.
- `fixture_copy_whitelist_array.rs` — `[u8; 32]` field → `copy`.
- `fixture_copy_whitelist_option_prelude.rs` — `Option<u32>` field
  → `copy`.
- `fixture_copy_whitelist_option_qualified.rs` —
  `core::option::Option<u32>` field → `copy`.
- `fixture_copy_whitelist_user_type.rs` — `MyCopyThing` field even
  with `#[derive(Copy)]` visible → `unknown_copy`.
- `sm-refactor-rust` updated with the borrow_context + Copy
  whitelist documentation.

**Follow-ups.**
- The `captured_self_fields` summary is per-site only; no
  per-field rollup is computed (intentional per Codex round-1).

---

### Phase RX-A1c — `unresolved_callbacks`

**Scope.** Collect every `self.<m>()`, `Self::<m>(…)`, or unqualified
method-call shape in moved bodies into `unresolved_callbacks`. No
resolution attempt — site list only.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1
`unresolved_callbacks` section.

**Components.**
- Body walk collecting call-shape sites: `self.METHOD(args)`,
  `Self::METHOD(args)`, `Self::METHOD::<…>(args)`, method-reference
  `self::METHOD` / `Self::METHOD`.
- Each entry: `{method, call_sites: [{line, column, in_method,
  context}]}`.
- Same-name de-dup: multiple sites of the same method roll up
  into one entry with N call_sites.
- Best-effort project-local-syntactic-index check: if the planner
  can show a same-file or sibling-module inherent method by that
  name, no special signal — the call is still in
  `unresolved_callbacks` until RX-R2 (`rust_ra_classify_callbacks`)
  promotes it.

**Gates.**
- `fixture_callback_self_method.rs` — `self.helper()` → one entry
  with one call site.
- `fixture_callback_self_associated.rs` — `Self::CONST_FN()` →
  one entry.
- `fixture_callback_method_reference.rs` — `self::handle` /
  `Self::handle` method-reference syntax → entries with
  `context: method_reference`.
- `fixture_callback_dedup.rs` — three calls to the same method →
  one entry with three call_sites.

**Follow-ups.**
- RX-R2 promotes entries to `resolved_callbacks`.

---

### Phase RX-A1d — `inherited_generics` + `inherited_bounds` + `captured_lifetimes`

**Scope.** Walk moved-method signatures and bodies for references
to type parameters, where-clause bounds, and explicit lifetimes
declared on the host impl block. Report; do not auto-inject.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1
`inherited_generics` / `inherited_bounds` / `captured_lifetimes`.

**Components.**
- Parse the enclosing `impl<...>` block params from the source's
  AST.
- For each moved method, collect identifiers used in its signature
  + body that match the impl's type-param names or lifetime names.
- Report unique set with `name` + `kind` (`type_param` |
  `lifetime`) + `bounds` (where-clause excerpts).

**Gates.**
- `fixture_inherited_generic.rs` — `impl<T: Send> Foo<T> { fn
  do_thing(&self, x: T) {...} }` → reports `T: Send` in
  `inherited_generics`.
- `fixture_inherited_lifetime.rs` — `impl<'a> Foo<'a> { fn use_ref(&self)
  -> &'a str {...} }` → reports `'a` in `captured_lifetimes`.
- `fixture_inherited_where_clause.rs` — bounds in `where` clause
  picked up under `inherited_bounds`.

**Follow-ups.**
- Auto-injection into the target impl block (open design question
  per parent doc); not in v1.

---

### Phase RX-A2 — FIXME marker grammar (plan-only) + apply refusal

**Scope.** Emit `// FIXME(refactor-plan-only): …` markers in the
would-be-generated target text when `deep_analysis: true` AND the
plan is in `status: blocked`. Plan-only markers exist in saved plan
JSON's target text only; the apply path refuses applied plans in
blocked state.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1 "FIXME marker
grammar"; "FIXME grammar default".

**Components.**
- Target-text generator (in `src/refactor/rust.rs`) gains a marker
  emission pass. For each entry in `captured_self_fields`,
  `unresolved_callbacks`, `inherited_generics` / `inherited_bounds`,
  the pass writes the appropriate
  `// FIXME(refactor-plan-only): …` line above the reference site
  in the target text.
- Stable grammar: `// FIXME(refactor-plan-only): <category>
  \`<name>\` — <description>. resolutions: <hints>.`
- Plan response carries
  `fixme_count: {plan_only: N, warning: 0}`. (The `warning` slot is
  zero in this phase; populated in RX-W1.)
- Plan `status` field: `Planned` | `Applied` | `Blocked` |
  `Errored`. A plan with non-zero `plan_only` FIXMEs and no
  applied edits goes to `Blocked`; an apply attempt on a Blocked
  plan returns `error.bad_state(code=plan_blocked)`.
- `sm-refactor-rust` updated with the marker grammar + blocked-
  plan semantics.

**Gates.**
- Plan with one `captured_self_field` + apply attempt → apply
  refused with `plan_blocked`.
- Saved plan JSON's target text contains the marker line.
- Fixture: every deep-analysis entry produces exactly one marker
  above its reference site.
- Regression: a clean plan (no deep_analysis findings) applies
  normally; no FIXME strings appear in the worktree post-apply.

**Follow-ups.**
- Warning grammar via RX-W1a / RX-W1b.

---

## State-extract surface

### Phase RX-S1 — `move_rust_struct_fields` (incl. `remaining_source_accessors`)

**Scope.** New plan kind. Move named fields between structs;
`deep_analysis: true` reports `remaining_source_accessors`.

**Realizes.** `design/archive/refactor-rust-expansion.md` §2.

**Components.**
- Plan kind dispatched from the plan dispatcher in
  `src/refactor/mod.rs`. Inputs: `source`, `target`, `impl_name` /
  `module_name`, `item_names`, `visibility`, `acknowledge_repr:
  bool`.
- Field-decl removal + insertion (FileEdits, hash-checked).
- `#[repr(...)]` detection: refuse when non-default repr detected
  unless `acknowledge_repr: true`. Operator-authority opt-out
  semantics per RX-V1.
- Generics propagation: `inherited_generics` (same shape as A1d)
  reports type params / bounds the target struct needs but doesn't
  declare. Reported, not auto-injected.
- `deep_analysis: true` walks the source struct's remaining
  methods for each moved field, classifying remaining accesses as
  `read | write | pattern_destructure | spread` with `line,
  column, context`.
- `sm-refactor-rust` updated.

**Gates.**
- Clean move (no remaining accessors) round-trips.
- Move with remaining accessors reports each site with correct
  classification.
- Repr-tagged struct refuses without `acknowledge_repr`.
- Repr-tagged struct with `acknowledge_repr: true` proceeds.
- Generics propagation reported for moved fields whose types
  reference the source impl's type params.
- `..rest` spread site flagged as `kind: spread`, NOT in FileEdits.

**Follow-ups.**
- Cross-file generic-conflict detection (target already declares
  `<T>` with different bounds → refuse).

---

### Phase RX-S2a — `add_rust_delegate_field`

**Scope.** Mechanical field + constructor-wire plan kind. Low-risk;
separated from `update_rust_callers` because that one is the risky
rewrite engine.

**Realizes.** `design/archive/refactor-rust-expansion.md` §3
`add_rust_delegate_field` half.

**Components.**
- Plan kind. Inputs: `source`, `impl_name` / `module_name`,
  `delegate_field`, `delegate_type`, `visibility`, `item_names`
  (constructor names to disambiguate; required when multiple).
- Appends `<vis> <name>: <Target>` to the struct decl.
- Wires `self.<name> = <Target>::new(...)` (or operator-provided
  construction expr) into the named constructor body. Refuse when
  no matching constructor exists.
- `sm-refactor-rust` updated.

**Gates.**
- Struct gains the field; constructor body gains the assignment.
- Multiple constructors with one matching name: routed to that
  constructor only.
- Struct with zero constructors: refuses with clear error.

**Follow-ups.**
- Pairs naturally with RX-S2b.

---

### Phase RX-S2b — `update_rust_callers` (conservative rewrite)

**Scope.** The risky rewrite engine. Per the design doc's
conservative table: only Copy-whitelisted rvalue reads and
unambiguous method calls are rewritten; everything else goes to
`unrewriteable_accessors`.

**Realizes.** `design/archive/refactor-rust-expansion.md` §3
`update_rust_callers` half.

**Components.**
- Plan kind. Inputs: `source`, `delegate_field`, `item_names`
  (moved methods + moved fields), `emit_applied_markers: bool`
  (default false; see RX-W1).
- Walk source file for `self.<x>` / `self.<m>(...)` shapes:
  - **Rewrite**: rvalue read of field whose type matches the Copy
    whitelist (reuse the A1b classifier). `self.field` →
    `self.delegate.field()`.
  - **Rewrite**: method call where method is in moved set.
    `self.m(args)` → `self.delegate.m(args)`.
  - **Rewrite**: method-reference syntax. `self::m` /
    `self.m` (in reference position) → `self.delegate.m`.
  - **Report only** under `unrewriteable_accessors`: field writes,
    compound writes, increment/decrement, LHS-position references,
    `match` arm destructure patterns, `mem::take`/`replace`/`swap`
    on the field, `..self` spreads.
- `borrow_promotions` report for sites where the rewrite would
  require `&mut self` on a path that had `&self`. Reported; the
  compiler will surface.
- `sm-refactor-rust` updated with the conservative-rewrite table.

**Gates.**
- `fixture_rewrite_copy_rvalue.rs` — `let n = self.count;`
  rewrites cleanly.
- `fixture_unrewriteable_write.rs` — `self.count = 5;` appears in
  `unrewriteable_accessors`, NOT in FileEdits.
- `fixture_rewrite_method_call.rs` — `self.helper()` rewrites
  cleanly when helper is in moved set.
- `fixture_unrewriteable_compound.rs` — `self.count += 1;` in
  `unrewriteable_accessors`.
- `fixture_unrewriteable_spread.rs` — `let Foo { a, .. } = self;`
  in `unrewriteable_accessors`.
- `fixture_borrow_promotion.rs` — read site whose rewrite needs
  `&mut self` appears in `borrow_promotions`.
- `fixture_nested_self_call.rs` — `self.method(self.field)` where
  both inner and outer match rewrite criteria. v1 conservative
  behavior: rewrite ONLY the outer site, push inner to
  `overlapping_rewrite_site`. Edit set MUST contain no overlapping
  byte ranges.
- `fixture_nested_self_with_clone.rs` —
  `self.method(self.field.clone())` — inner site reported as
  `overlapping_rewrite_site`; the `.clone()` chain belongs to the
  inner expression and is not separately rewritten.
- `fixture_args_with_self_method.rs` —
  `foo(self.method(), self.field)` — both args separately
  reportable; conservative v1 rewrites only the args that don't
  share a parent rewrite-target.
- Edit-overlap invariant: the FileEdit set produced by
  `update_rust_callers` is guaranteed non-overlapping. The
  pessimistic refusal path is the safety mechanism.
- Integration: RX-S1 + RX-S2a + RX-S2b + `cargo check` on a small
  struct compiles after apply.

**Follow-ups.**
- Auto-generating accessors on the target — gated by
  `rust_public_api_guard` (RX-G2); deferred.

---

## Trait extraction

### Phase RX-T1 — `extract_rust_trait` (structural object-safety report)

**Scope.** New plan kind. Lifts a method subset into a trait, adds
`impl Trait for Struct`. Structural object-safety check;
`semantic_status: IndexedHints`.

**Realizes.** `design/archive/refactor-rust-expansion.md` §4.

**Components.**
- New plan kind. Inputs: `source`, `target`, `module_name`,
  `impl_name`, `item_names`.
- Trait-decl generation preserving generics, where-clauses, `async`,
  lifetimes.
- `impl <Trait> for <Struct>` wrapping original bodies.
- `: Sized` added when lifted methods take/return `Self` by value;
  `dyn_compatible: false` reported.
- Structural `object_safety_report`: no generic methods, no `Self`
  by value, no associated constants. Reports findings; does not
  refuse on this alone.
- `call_site_warnings` (under IndexedHints): syntactic find for
  `Struct::method(...)` UFCS and `<Struct as Trait>::method`
  qualified paths referencing lifted methods. List, not rewrite.
- `trait_in_scope_required` listing module paths that now need
  `use <trait_path>::<TraitName>;`. List, not rewrite.
- Refusal: a method whose body calls a non-lifted, non-public
  inherent method on `self` refuses cleanly.
- `sm-refactor-rust` updated.

**Gates.**
- Lift two methods → trait file generated, impl block created,
  source impl loses methods.
- `Self`-by-value method gets `: Sized` and `dyn_compatible:
  false`.
- Body calling non-lifted private method refuses with the right
  error code.
- Object-safety report fires on generic-method case.
- UFCS call sites listed in `call_site_warnings`.

**Follow-ups.**
- RA-backed object-safety verification (`rust_ra_extract_trait`
  variant, future).
- Auto-injecting `use Trait;` at sites — gated by
  `rust_public_api_guard`.

---

## Type-use migration

### Phase RX-M1 — `migrate_rust_type_usages` with `replacement_kind` enum

**Scope.** Restructure (per Codex round-2): `replacement_kind` enum,
per-site legality reporting rather than free-text replacement.

**Realizes.** `design/archive/refactor-rust-expansion.md` §5.

**Components.**
- Plan kind. Inputs: `source`, `module_name` (old type),
  `replacement_kind` enum (`BareConcrete`, `BoxDyn`, `ArcDyn`,
  `RcDyn`, `ImplTrait`, `GenericParamTBoundedTrait`), `new_text`
  (replacement expression).
- Per-`replacement_kind` legality table per the design doc.
  Walker classifies each candidate site by syntactic position;
  edit or push to `migration_skipped`.
- Skip reasons: constructor / associated-item paths, turbofish,
  qualified `<<old> as Trait>::method`, `TypeId::of`, pattern
  positions, detected local shadowing.
- For `GenericParamTBoundedTrait`: emit additional FileEdits for
  the enclosing item's generics + where clause; refuse on
  pre-existing `<T>` conflict.
- `sm-refactor-rust` updated.

**Gates.**
- Each `replacement_kind` has fixture coverage.
- `ImplTrait` at struct field refuses with the right error code.
- `GenericParamTBoundedTrait` with no `<T>` conflict edits
  generics; with conflict refuses.
- Skipped sites enumerated with reasons.

---

## LSP-backed plan kinds

Depend on the warm `LspSessionManager` (`src/refactor/rust.rs:1939`
rename + `:1991` organize-imports). Per the cross-surface invariant,
they fail closed if rust-analyzer is unavailable; no silent
downgrade.

### Phase RX-R1 — `rust_ra_move_item_to_module`

**Scope.** RA-backed move of top-level items (free fns, types,
consts/statics, modules). Items only for v1; impl methods stay with
`extract_rust_impl_methods` per Codex round-3.

**Realizes.** `design/archive/refactor-rust-expansion.md` §8.

**Components.**
- New plan kind. Inputs: `source`, `target`, `item_names`,
  `item_kinds`.
- Reject `item_kinds=["impl_method"]` with explicit redirect to
  `extract_rust_impl_methods`.
- Routes through `LspSessionManager`; calls
  `textDocument/codeAction` requesting "Move to module" code
  action and resolves the resulting workspace edit.
- Workspace edits converted to FileEdits with hash checks.
- `semantic_status: LspVerified`.
- Fail-closed: LSP unavailable → `error.lsp_unavailable`. No
  fallback. Per RX-V3 cross-surface invariant.
- `sm-refactor-rust` updated.

**Gates.**
- Move a free fn → workspace edits applied, callers updated,
  imports fixed.
- Move a type → same.
- `item_kinds=["impl_method"]` rejects with redirect.
- LSP unavailable → `error.lsp_unavailable`.
- Hash check fails on stale source → standard refusal.

**Follow-ups.**
- Future `rust_ra_move_impl_method_to_module` once RA exposes
  precise code action for impl methods.

---

### Phase RX-R2 — `rust_ra_classify_callbacks` (consumes RX-A1c)

**Scope.** RA-backed resolution of `unresolved_callbacks` produced
by RX-A1c. Promotes the deep_analysis report's `semantic_status` to
`LspVerified` for the resolved subset.

**Realizes.** `design/archive/refactor-rust-expansion.md` §9.

**Components.**
- New plan kind. Inputs: `source`, `item_names` (methods whose
  bodies to analyze), or a plan-ref to a prior
  `extract_rust_impl_methods` step.
- For each call site, `textDocument/references` +
  `textDocument/definition` to resolve declaring item.
- Classify: `Inherent`, `TraitImpl`, `BlanketImpl`, `DerefTarget`,
  `External`.
- Populates `resolved_callbacks` on the plan response with
  `{method, declaring_item, declaring_kind, call_sites}`.
- Fail-closed on LSP unavailable.
- `sm-refactor-rust` updated.

**Gates.**
- Inherent-method call site classified `Inherent`.
- Trait-method call site classified `TraitImpl` with trait name.
- Std-library call site classified `External`.
- Empty input → empty output, not an error.

**Follow-ups.**
- Run as a sub-step inside `extract_rust_impl_methods` when both
  appear in the same run (orchestrator/atom decision).

---

## Compile-fix round

### Phase RX-C1 — `rust_compile_fix_round` plan kind

**Scope.** The repair primitive. Consumes captured rustc/RA
diagnostics from RX-F2a/F2b's runner extension; produces a
reviewable `RefactorPlan`. **Depends on RX-F2b for the obligation
machinery.**

**Realizes.** `design/archive/refactor-rust-expansion.md` §10 planner
extension subsection.

**Components.**
- Plan kind. Inputs: `diagnostics_ref` (default `"last"`),
  `project_dir`, optional `restrict_to_files`.
- Reads diagnostics from the run-context scratch struct (RX-F2a)
  under the obligation machinery (RX-F2b).
- Per-diagnostic classifier. Codex round-7 push: don't key only on
  error code; also consult `spans` and `children` (suggestions)
  from the rustc JSON. The classifier:
  - `E0432` unresolved import / `E0433` unresolved path or module
    → propose `add_rust_use_decl`. Use the diagnostic's
    `suggested_replacement` child when present.
  - `E0603` module/function/method is private / `E0624` method is
    private / `E0616` field is private → propose
    `rewrite_rust_item_visibility` to `pub(crate)`. Operator
    review required (not auto-applied without acknowledgment).
  - `E0599 no method named X` /
    `trait bounds not satisfied with the help of an import` → if
    the trait owning X is in the project, propose
    `add_rust_use_decl` for the trait.
  - `E0277 the trait bound ... is not satisfied` → `leftovers`,
    diagnostic preserved.
  - `E0382 use of moved value` / `E0502 cannot borrow ... as mutable`
    / other borrow-checker errors → `leftovers`.
  - `E0061 wrong number of arguments` (common after RX-T1 / RX-E1
    signature changes) → `leftovers` UNLESS rustc provides a
    machine-applicable `suggested_replacement` whose span matches a
    plan-touched span; in the narrow matching case, propose a
    `replace_text` step. Default: leftovers.
  - `E0308 mismatched types` (common after RX-E1 / RX-M1 migrations)
    → `leftovers` by default. E0308 is usually semantic intent,
    not mechanical patching; auto-repair is unsafe in v1.
  - Unrecognized → `leftovers`.
- Produces a `RefactorPlan` with proposed steps. Reviewable,
  hash-checked.
- Marks the obligation `Consumed` after running. Leftovers leave
  the obligation as `LeftOver` (acceptable for commit).
- `semantic_status: LspVerified`.
- `sm-refactor-rust` updated with the classifier coverage list and
  the leftover semantics.

**Gates.**
- E0432/E0433 unresolved-import case → plan adds `use` decl using
  the diagnostic's `suggested_replacement`.
- E0603/E0624/E0616 privacy case → plan rewrites visibility (not
  auto-applied without op acknowledgment).
- E0277 trait-bound case → `leftovers`; never auto-repaired.
- E0382/E0502 borrow-checker → `leftovers`.
- E0061 wrong-args case → `leftovers` UNLESS span-matching
  machine-applicable suggestion exists.
- E0308 mismatched-types case → `leftovers`.
- Plan with diagnostics where suggestions are present uses the
  span/suggestion data, not just the code.
- Run with compile_fix that resolves all diagnostics commits.
- Run with compile_fix that leaves leftovers commits if later
  validation passes.
- Run with compile_fix that produces nothing AND no leftovers
  fails (obligation unconsumed).

**Follow-ups.**
- Loop primitive: open design question. v1 is one round.

---

## Error-type migration (narrowed; consumes RX-C1)

### Phase RX-E1 — `rewrite_rust_error_type` narrowed

**Scope.** Per Codex round-1: signatures + literal construction
rewrites only. `?`-site conversion gaps surface via RX-C1's
compile-fix round, which this plan kind composes after.

**Realizes.** `design/archive/refactor-rust-expansion.md` §6.

**Components.**
- Plan kind. Inputs: `source`, `item_names`, `old_text`, `new_text`,
  `error_mapping`, `acknowledge_public_api_change: bool`.
- Return-type rewrite for named functions.
- Strict string-match rewrite of construction forms per
  `error_mapping`.
- `question_mark_sites` report with `text_compatible | unknown`
  classification (hint only).
- Refuse on `downcast` / `downcast_ref` in named functions.
- `acknowledge_public_api_change` follows the operator-authority
  invariant (RX-V1).
- `sm-refactor-rust` updated.

**Gates.**
- Function signatures rewrite.
- `bail!(OldErr::IoFail)` → `bail!(NewErr::Io)` via mapping.
- Unmapped construction left unchanged.
- `?` sites enumerated with classification.
- `downcast` usage refuses with clear error.
- When the error type is `pub` and operator did not set
  `acknowledge_public_api_change`, the atom-side guard
  (via RX-G2) blocks; direct callers see a warning.

---

### Phase RX-L1 — `lift_rust_inherent_to_free`

**Scope.** Move impl methods whose bodies don't read `self` into
free functions in a child module; rewrite call sites.

**Realizes.** `design/archive/refactor-rust-expansion.md` §7.

**Components.**
- Body walk for `self.<x>` / `Self::<x>` (other than `Self` in
  type position). Any such reference refuses the lift for that
  method.
- Free-fn generation: `pub(crate) fn <name>(<args-minus-self>) ->
  <ret> { … }`.
- Call-site rewrites.
- Lifetimes preserved verbatim; never re-elide.
- `Self` in type position rewritten to concrete struct name on
  target.
- `sm-refactor-rust` updated.

**Gates.**
- Pure helper method (zero self reference) lifts cleanly.
- Method with `self.field` refuses.
- Method with `Self::CONST` refuses (Self → concrete is only safe
  for type positions, not paths).
- Lifetimes preserved verbatim.
- Call sites rewritten.

---

## Analysis-only plan kinds

### Phase RX-G1 — `rust_impl_partition_analysis`

**Scope.** Graph-only output. Methods, fields, edges. Clustering
is a SEPARATE concern.

**Realizes.** `design/archive/refactor-rust-expansion.md` §11.

**Components.**
- Plan kind. Inputs: `source`, `impl_name`.
- Walks the impl; per-method extracts:
  `{name, attrs, router, reads, writes, calls,
  unresolved_callbacks}`.
- Field section: name, type, shared_by.
- Edges: `{from, to, kind: reads|writes|calls}`.
- `semantic_status: IndexedHints`.
- No FileEdits.
- `sm-refactor-rust` updated.

**Gates.**
- Run on `BlackboxServer` impl in `src/main.rs` produces a graph
  with all 30+ methods and their edges.
- `#[tool]` attribution carried through to per-method `router`.
- JSON output deterministic (ordered by source position).

---

### Phase RX-G2 — `rust_public_api_guard`

**Scope.** Precondition analysis for plans touching `pub` items.
No FileEdits; emits structured deltas.

**Realizes.** `design/archive/refactor-rust-expansion.md` §13.

**Components.**
- Plan kind. Inputs: `source` (file or directory),
  `proposed_changes`.
- Walks `pub` items; reports:
  `public_items_touched`, `public_api_delta_summary`,
  `crate_root_re_exports_affected`, `advisory_severity`.
- `sm-refactor-rust` updated.

**Gates.**
- Modifying a `pub fn` signature flags `breaking`.
- Adding a `pub fn` flags `info`.
- Deleting a `pub use` at crate root flags `breaking`; re-export
  listed.

**Follow-ups.**
- Cargo-semver-checks integration (future).

---

## Domain-specific plan kind

### Phase RX-P1 — `rust_match_arm_to_strategy`

**Scope.** Hybrid restructuring per Codex round-2: ProviderSpec
data + ProviderDriver behavior modules from a match-on-enum shape.
Targeted at `src/orchestration/providers.rs`.

**Realizes.** `design/archive/refactor-rust-expansion.md` §12.

**Components.**
- Plan kind. Inputs: `source`, `enum_name`,
  `behavior_family_names`, `data_field_names`,
  `driver_share_groups`, `driver_name` (optional).
- Per-variant module file generation: spec constants + driver
  trait impl.
- Router function on the enum dispatching by variant.
- Driver-family sharing: shared driver module with per-variant
  spec configuration.
- Refusal: variants with non-trivial associated data refuse;
  operator handles by hand.
- `sm-refactor-rust` updated.

**Gates.**
- Apply to `Provider` enum: one-driver-per-variant case generates
  N files.
- `driver_share_groups: [["Glm","Deepseek","Inception"]]`:
  shared driver module generated.
- Variant with `Foo(Bar)` data refuses.

---

## Applied warning markers (split per Codex round-7)

### Phase RX-W1a — Warning marker grammar infrastructure

**Scope.** The `// FIXME(refactor-warning): …` prefix +
`emit_applied_markers: bool` flag plumbing on plan kinds that may
emit warnings. Marker emission only; per-kind warning-site
detection is RX-W1b.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1 FIXME marker
grammar (warning prefix).

**Components.**
- Stable warning grammar:
  `// FIXME(refactor-warning): <category> — <description>. <hints>.`
- `emit_applied_markers: bool` flag on plan kinds whose deep_analysis
  may produce warning-category findings (RX-S2b is the v1 producer;
  RX-S1 may add mutable-capture warnings as a follow-up).
- Marker emission helper writes `// FIXME(refactor-warning): …`
  above FileEdit-targeted lines.
- `fixme_count.warning` populated.
- Validation: warning markers MUST be above code that parses
  successfully. The apply path verifies post-edit parse before
  committing.
- `sm-refactor-rust` updated.

**Gates.**
- Stable grammar emitted for one synthetic warning case.
- `emit_applied_markers: false` (default) → no warning markers.
- Apply with warning markers succeeds (compiling code).
- `fixme_count.warning` correctly counted.

---

### Phase RX-W1b — Per-kind warning-site detection (initial: RX-S2b borrow_promotions)

**Scope.** Wire `update_rust_callers`'s `borrow_promotions` report
to emit warning markers when `emit_applied_markers: true`.

**Realizes.** `design/archive/refactor-rust-expansion.md` §1 FIXME marker
grammar applied case.

**Components.**
- `update_rust_callers` (RX-S2b) calls the warning-marker helper
  (RX-W1a) for each `borrow_promotions` site when
  `emit_applied_markers: true`.
- Marker text:
  `// FIXME(refactor-warning): borrow promotion — this delegate
  access now goes through `&mut self.<delegate>` even though the
  original read was through `&self.<field>`. cross-check no
  concurrent borrow.`

**Gates.**
- `update_rust_callers` with `emit_applied_markers: true` emits
  warning marker above each borrow_promotions site.
- `update_rust_callers` with `emit_applied_markers: false`
  emits no markers but still populates `borrow_promotions` in the
  response.

**Follow-ups.**
- Additional producers when later plan kinds gain warning-category
  findings (RX-S1 mutable captures becoming `final`-equivalent;
  RX-E1 error-conversion gaps the compiler doesn't catch).

---

## Cross-surface invariants (documentation; runtime enforcement is v2)

### Phase RX-V1 — Operator-authority opt-out invariant

**Scope.** Document the operator-authority invariant for
`acknowledge_repr` (RX-S1) and `acknowledge_public_api_change`
(RX-E1 and others). Atoms must pass through, never default or
infer. v1 is doc + the response field below; runtime enforcement
is v2.

**Realizes.** `design/archive/refactor-rust-expansion.md` "Cross-Surface
Invariants — Operator-authority opt-outs".

**Components.**
- `sm-refactor` entry (cross-language) stating the invariant.
- `operator_opt_outs_used: ["acknowledge_repr",
  "acknowledge_public_api_change", …]` audit field lives on the
  durable `RefactorPlan` (not just the `RefactorPlanSummary`), so
  plans saved via `output_path` preserve the audit trail. Summary
  views copy the field out for the inline response. Codex round-2:
  audit on summary-only loses the trail in saved plan files.
- v2 placeholder: dispatch-side check that an agent's invocation
  passes these flags only from declared `inputs`, never as a
  constant. Requires per-dispatch tool-call provenance (out of
  scope; tracked as v2).

**Gates.**
- Plan response surfaces `operator_opt_outs_used` when any flag
  is set.
- `sm-refactor` entry exists.

---

### Phase RX-V2 — Atom command-allowlist invariant

**Scope.** Document the cargo-only command allowlist for
atom-dispatched `bbox_refactor_run` invocations. v1 is doc +
atom-prompt-template encoding; v2 adds runner-side enforcement.

**Realizes.** `design/archive/refactor-rust-expansion.md` "Cross-Surface
Invariants — `bbox_refactor_run` command step allowlist for atoms".

**Components.**
- `sm-refactor` entry.
- Atom prompt templates (authored in `design/refactor-agents-impl.md`)
  encode the allowlist.
- v2 placeholder: runner accepts `dispatch_origin: "agent" |
  "operator"` flag set by `bro_agent_dispatch`; when `agent`,
  runner enforces server-side.

**Gates.**
- `sm-refactor` entry exists.

---

### Phase RX-V3 — Fail-closed invariant for RA-backed plan kinds

**Scope.** Document and enforce that `rust_lsp_rename`,
`rust_organize_imports`, `rust_ra_move_item_to_module` (RX-R1),
`rust_ra_classify_callbacks` (RX-R2) fail closed when
rust-analyzer is unavailable. No silent downgrade to syntax-only.

**Realizes.** `design/archive/refactor-rust-expansion.md` "Cross-Surface
Invariants — RA-backed plan kinds fail closed".

**Components.**
- LSP-backed plan-kind dispatchers explicitly check session
  availability before issuing the LSP request. On unavailability
  (binary missing, init timeout, crashed mid-run), return
  `error.lsp_unavailable` with the underlying cause.
- Never fall back to a non-LSP code path within the same kind.
- `sm-refactor-rust` updated.

**Gates.**
- Each LSP-backed kind's "LSP unavailable" path tested via fixture
  with rust-analyzer binary unset.
- Error code is stable: `error.lsp_unavailable`.

---

## Phase dependency DAG

```
Substrate:
  RX-F1a (semantic_status) ─┐
  RX-F1b (plan slot)       ─┼─► every later phase (plan response shape + slot policy)
                            │
  RX-F2a (capture)         ─┤
  RX-F2b (continue_for_repair + obligations) ──► RX-C1

Deep-analysis surface:
  RX-A1a (Rust wiring) ──► RX-A1b (captured_self_fields + Copy)
                       ──► RX-A1c (unresolved_callbacks) ──► RX-R2 (resolved_callbacks)
                       ──► RX-A1d (generics/lifetimes)
  RX-A1b/c/d           ──► RX-A2 (FIXME plan-only markers)

State-extract:
  RX-A1b (Copy whitelist reuse) ──► RX-S2b (conservative rewrite)
  RX-S1 (move fields) ──► RX-S2a (delegate field) ──► RX-S2b (callers)

Trait + types:
  RX-T1 (independent)
  RX-M1 (independent)

LSP-backed:
  RX-R1, RX-R2 (depend on existing LspSessionManager; RX-V3 fail-closed invariant)

Repair:
  RX-F2b ──► RX-C1 (compile-fix needs obligations machinery; build-time)

  Runtime composition (NOT build-time deps; phases land independently):
    RX-E1 + RX-C1 — error migration uses compile-fix to repair ?-sites
    RX-S2b + RX-C1 — unrewriteable accessors may surface via compile-fix
    RX-T1 + RX-C1 — trait extraction's call-site warnings consumed

Analysis-only:
  RX-G1 (impl partition graph) — independent
  RX-G2 (public-api guard) — independent

Domain:
  RX-P1 (match-arm-to-strategy) — independent

Warnings:
  RX-A2 (marker emission infra) ──► RX-W1a (warning grammar) ──► RX-W1b (RX-S2b producer)
  RX-S2b ────────────────────────────────────────────────────────► RX-W1b

Invariants (documentation):
  RX-V1, RX-V2, RX-V3 — gates for agents-impl
```

## Non-goals (this skeleton)

- v2 runtime enforcement of operator-authority and command-allowlist
  invariants. v1 ships documented form + audit fields (RX-V1
  `operator_opt_outs_used`).
- Macro-expansion-aware analysis. Out per design.
- Workspace-wide multi-crate type indexing. Project-local only.
- Auto-detection of `driver_share_groups` in RX-P1. Operator passes
  explicit groups in v1.
- `dejunk_rust_struct` modernization plan kind — separate doc.
- Cargo-semver-checks integration in RX-G2 — future.
- Loop semantics inside RX-C1 (multi-round repair) — open question.

## Known follow-ups across phases

- Bench every deep_analysis pass on `src/main.rs`'s `BlackboxServer`
  impl. Target: full deep_analysis + FIXME emission in seconds.
- Atomic agents from `design/refactor-agents.md` need RX-F1a/F1b,
  RX-F2a/F2b, RX-A1a/b/c/d, RX-A2, RX-C1, RX-V1/V2/V3 minimum.
  Per atom additional deps:
  - `rust-state-extract` → RX-S1, RX-S2a, RX-S2b, RX-W1a, RX-W1b.
  - `rust-trait-from-impl` → RX-T1, RX-M1.
  - `rust-error-migrate` → RX-E1, RX-G2.
  - `rust-test-island-extract` → no new Rust phases (uses
    `extract_rust_items` already in production).
  - `rust-impl-partition-graph` → RX-G1.
  - `rust-public-api-guard` → RX-G2.
  - `rust-split-god-impl` → RX-R2 (resolved_callbacks) if it
    promises resolved-callback output.

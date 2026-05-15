---
title: "Refactor Agents - implementation skeleton"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
  - atoms
brief: "Implementation skeleton for refactor atom manifests layered over Rust expansion and agent-system substrate."
---

# Refactor Agents — implementation skeleton

Companion to `refactor-agents.md`. Each phase names a discrete
implementation chunk: scope, realizes, components, gates, known
follow-ups. Phases are dependency-ordered. No timelines — landing a
phase unblocks dependents; landing all phases realizes the design.

This skeleton depends on `rust/refactor-rust-expansion-impl.md`
(prefix `RX-`). Minimum dependencies before any atom-impl phase
lands:

- RX-F1a (`semantic_status` migration)
- RX-F1b (plan-file slot policy)
- RX-F2a (runner `capture` plumbing)
- RX-F2b (`continue_for_repair` obligations)
- RX-A1a (Rust `deep_analysis` wiring)
- RX-A1b/c/d (deep_analysis field populators) — `rust-split-god-impl`,
  `rust-state-extract` use them
- RX-A2 (FIXME plan-only markers; required for `status: Blocked`
  semantics atoms rely on)
- RX-C1 (`rust_compile_fix_round`)
- RX-V1 (operator-authority opt-out invariant doc)
- RX-V2 (command-allowlist invariant doc)
- RX-V3 (RA-backed fail-closed invariant doc)

Per-atom additional dependencies are listed in each phase.

Phases are prefixed `RA-` to disambiguate from `RX-` (Rust expansion),
`AS-` (agent system), and other plan prefixes.

This skeleton also assumes the existing agent infrastructure
(`design/orchestration/agents/agent-system.md` and `design/orchestration/agents/agent-system-impl.md` Phases
AS-D1 through at least AS-I2) has landed: `ArtifactKind::Agent`,
`bro_agent_*` MCP tools, manifest schema validation, embedding bucket
`agent_manifest`, and dispatch path at
`src/tools/agents.rs::bro_agent_dispatch`. The refactor-atom layer
ships JSON manifests installed via `bbox_artifact_install`; no new
agent-system infrastructure is required.

---

## Substrate phases — brofiles + shared atom contract

These three phases provide the prerequisites every atom needs.

### Phase RA-B1 — `rust-refactor-persona` brofile

**Scope.** Author the narrow `rust-refactor-persona` brofile whose
allow list contains only the refactor + grounding tool set, denying
`Write` / `Edit` / `Bash` / `bbox_learn` / `bbox_remember` /
`bbox_decide` / `bbox_forget` / `bbox_render` / `bro_*`. Ship as a
shipped artifact (see follow-up below for the alternative).

**Realizes.** `design/refactor-agents.md` "Prerequisite: the narrow
refactor brofiles — `rust-refactor-persona`".

**Components.**
- Brofile JSON authored at `examples/brofiles/rust-refactor-persona.json`
  matching the design-doc spec. Filter strings use the canonical
  MCP-prefix form because the daemon's filter merge keys on
  full pattern strings; the design doc's
  `mcp__blackbox__bbox_*` form is authoritative:
  - `provider: claude` (or operator choice), `model:
    claude-sonnet-4-6`, `effort: medium` as defaults — installer
    can re-target.
  - `filters.allow`:
    `mcp__blackbox__bbox_code_symbols`,
    `mcp__blackbox__bbox_code_node_describe`,
    `mcp__blackbox__bbox_code_query`,
    `mcp__blackbox__bbox_refactor_status`,
    `mcp__blackbox__bbox_refactor_project_refs`,
    `mcp__blackbox__bbox_refactor_plan`,
    `mcp__blackbox__bbox_refactor_apply`,
    `mcp__blackbox__bbox_refactor_run`,
    `mcp__blackbox__bbox_note`,
    `mcp__blackbox__bbox_thread`,
    `mcp__blackbox__bbox_pin`,
    `mcp__blackbox__bbox_inspect_entity`,
    `mcp__blackbox__bbox_hybrid_search`,
    plus `Read`, `Grep`, `Glob`.
  - `filters.disallow`:
    `mcp__blackbox__bbox_forget`,
    `mcp__blackbox__bbox_decide`,
    `mcp__blackbox__bbox_learn`,
    `mcp__blackbox__bbox_remember`,
    `mcp__blackbox__bbox_render`,
    `mcp__blackbox__bro_*`,
    `Bash`, `Write`, `Edit`.
  - `lens`: the prose specified in the design doc.
- Brofile installs via `bro_brofile(action="create", …)` or the
  shipped-artifact path (TBD per follow-up).
- Brofile validation: a unit test confirms the allow/disallow
  pattern set matches the design-doc spec exactly. Regressions
  detected mechanically.
- `sm-refactor` cross-language entry (NOT `sm-refactor-rust`)
  documents the brofile's existence and the narrow-allow invariant.

**Gates.**
- `bro_brofile(action="get", name="rust-refactor-persona")`
  returns the spec'd allow/disallow.
- Allow set is exactly the design-doc allow set (test fails on
  extra OR missing entries).
- Disallow set covers the non-negotiable list.
- `sm-refactor` entry exists.

**Follow-ups.**
- **Shipping policy** (design-doc open question 1): whether to
  ship this brofile pre-installed with the daemon, or require
  operators to install per-host. Default proposal in this phase
  is to ship under `examples/brofiles/` with an
  install-on-demand convention; the artifact catalog can later
  carry a brofile-install bootstrap that activates it.
- `effort: high` variant for atoms that need more model capacity
  (e.g., `rust-split-god-impl` on very large impls).

---

### Phase RA-B2 — `java-refactor-persona` brofile

**Scope.** Author the parallel Java brofile. Same allow/disallow
shape; Java-flavored lens.

**Realizes.** `design/refactor-agents.md` "Prerequisite: the narrow
refactor brofiles — `java-refactor-persona`".

**Components.**
- `examples/brofiles/java-refactor-persona.json` mirroring RA-B1's
  shape, with the lens prose adapted for Java (`mvn` / `gradle`
  validation language; reference to Lombok caveats).

**Gates.**
- Same gates as RA-B1 (allow/disallow exact-match, lens prose,
  sm entry).

**Follow-ups.**
- Required only if RA-X1 (Java appendix atom) ships. The brofile
  can be deferred until then.

---

### Phase RA-S1 — Refactor-atom manifest lint

**Scope.** Add a refactor-atom-specific lint pass on top of the
generic `kind="agent"` schema validation at
`schema/agent.schema.json` and `src/orchestration/agents/validate.rs:231`.
The generic path validates manifest shape and JSONSchema syntax;
this phase enforces invariants specific to the refactor-atom
contract.

**Realizes.** `design/refactor-agents.md` "Prerequisite: the narrow
refactor brofiles"; "Shared `prompt_template` shape"; "Operator-
authority invariant".

**Components.**
- **Lint trigger** (Codex round-2 fix): the lint runs when the
  manifest declares itself a refactor atom via a top-level
  marker, NOT when `brofile_ref` matches a refactor persona
  (the latter would skip the lint precisely when a manifest
  claims-to-be-refactor uses a permissive brofile — the case
  the lint must catch). The marker is one of:
  - Top-level `"_contract": "refactor-atom/v1"` in the manifest
    JSON. Authoring convention; refactor atom templates
    include this.
  - OR the manifest source path matches
    `examples/agents/refactor/**` (path-based trigger for the
    shipped catalog).
- **Severity split** (Codex round-2 fix):
  - **Hard reject** (install fails):
    - `brofile_ref` is not one of the recognized refactor
      personas (`rust-refactor-persona` or
      `java-refactor-persona`). Codex round-3: no escape-hatch
      flag in v1. The trigger conditions (`_contract:
      "refactor-atom/v1"` marker OR path under
      `examples/agents/refactor/**`) are themselves the
      opt-in. An operator who wants different semantics
      either doesn't declare the contract marker or hosts
      the manifest outside the refactor path. This avoids
      adding a top-level field that `schema/agent.schema.json`
      (which has `additionalProperties: false`) would reject.
    - `inputs.schema` declares an `acknowledge_*` opt-out
      field with a `default` value (any default; the field
      must be operator-explicit per the operator-authority
      invariant in RX-V1).
  - **Warning** (install succeeds with `install_warnings`):
    - `filter_overlay.allow` non-empty (overlay can only
      widen; refactor atoms should narrow via additional
      denies only).
    - `outputs.schema` missing one or more RA-T1 base fields
      (status, plan_path, files_touched, fixme_count,
      deep_analysis_summary, cargo_result, block_reason,
      done_note_id).
    - `inputs.prompt_template` missing one or more recognizable
      protocol markers: `bbox_refactor_plan`,
      `bbox_refactor_run`, `bbox_note(kind=`.
- Warnings surface in `install_warnings`
  (`src/orchestration/agents/registry.rs`). Hard rejects return
  `error.bad_input(code=refactor_atom_lint_failed)` with a
  specific reason.

**Gates.**
- Install a known-good refactor atom: lint emits zero warnings,
  install succeeds.
- Install a manifest with `"_contract": "refactor-atom/v1"` and
  `brofile_ref: "code-reviewer-persona"`: install REJECTED with
  `refactor_atom_lint_failed` (brofile_ref reason). No escape
  hatch in v1; the operator drops the contract marker if they
  want a non-refactor brofile.
- Install with `acknowledge_repr` having `default: false`: install
  REJECTED.
- Install with `filter_overlay.allow: ["mcp__blackbox__Bash"]`:
  install succeeds, `install_warnings` non-empty.
- Install with missing base outputs fields: install succeeds,
  warning.
- Install with prompt template missing protocol markers: warning.

**Follow-ups.**
- Future strict-mode flag that escalates warnings to rejections,
  gated by per-host config. Out of scope v1.

---

### Phase RA-T1 — Shared atom prompt-template + outputs.schema base

**Scope.** Author the shared template + base outputs.schema that
every refactor atom embeds. These are not infrastructure changes;
they are a documented contract that atom-authoring tooling can
reference.

**Realizes.** `design/refactor-agents.md` "Shared `prompt_template`
shape"; "Shared `outputs.schema` shape".

**Components.**
- Reference template authored as a text file at
  `examples/agents/refactor/_template.prompt.md`. Contains the
  five-step protocol (ground → plan(deep_analysis=true) →
  decide → apply-or-block → done-note) with `{{var}}` placeholders
  for atom-specific charter + inputs.
- Reference base outputs.schema authored at
  `examples/agents/refactor/_base.outputs.schema.json` per the
  design doc:
  ```json
  {
    "status": "...",
    "plan_path": "...",
    "files_touched": [...],
    "fixme_count": { "plan_only": 0, "warning": 0 },
    "deep_analysis_summary": {...},
    "cargo_result": {...},
    "block_reason": "...",
    "done_note_id": "..."
  }
  ```
- Both files are reference material; per-atom manifests embed the
  filled-in template and the union of `_base` + atom-specific
  schema. Mechanical "include" is not currently supported by the
  artifact installer — atoms inline their templates and schemas.
- Authoring helper (out of scope here, tracked as a follow-up):
  a small `tools/refactor-atom-fill` script that reads the
  template + the atom-specific schema and writes the final
  manifest, so atom maintenance doesn't drift.
- `sm-refactor` entry referencing the templates.

**Gates.**
- Template + base schema files exist with documented variable list
  and field list.
- One reference atom (RA-A1, first atom phase) uses the template
  successfully, demonstrating round-trip from template → manifest
  → install → dispatch.
- `sm-refactor` entry exists.

**Follow-ups.**
- Authoring helper script.
- Schema composition support in the artifact installer (later;
  v1 has manifests inline their full schemas).

---

## Atom phases — initial Rust catalog

Each phase ships one atom JSON manifest at
`examples/agents/refactor/<atom-name>.json`, installs it via
`bbox_artifact_install`, and validates via integration tests.

The phases are ordered by complexity and dependency: analysis-only
atoms first, then atoms that compose multiple plan kinds, ending
with the headline `rust-split-god-impl`.

### Phase RA-A1 — `rust-impl-partition-graph` (analysis-only)

**Scope.** First atom. Wraps `rust_impl_partition_analysis` (RX-G1)
with the atomic-agent contract. Analysis-only, no apply path.

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-impl-partition-graph`".

**Components.**
- Manifest at `examples/agents/refactor/rust-impl-partition-graph.json`:
  - `description`: "Produce a method/field/call graph for an impl
    block to support partitioning decisions."
  - `when_to_use`: standard list per the design-doc atom.
  - `anti_patterns`: standard list (do not use as a clustering
    algorithm).
  - `brofile_ref`: `rust-refactor-persona`.
  - `filter_overlay.disallow`: additional denies beyond the
    brofile — none specifically needed; the brofile already
    excludes everything mutating.
  - `inputs.schema`:
    ```json
    {
      "type": "object",
      "required": ["project_dir", "source_file", "impl_name"],
      "properties": {
        "project_dir": { "type": "string" },
        "source_file": { "type": "string" },
        "impl_name": { "type": "string" }
      }
    }
    ```
  - `inputs.prompt_template`: filled-in five-step protocol from
    RA-T1's reference template, with charter
    "Produce the partition graph for the named impl block. Do not
    propose partitions; the operator decides."
  - `outputs.schema`: base schema (RA-T1) extended with the
    graph fields (`methods`, `fields`, `edges`) per RX-G1's
    response shape.
  - `composition.parallel_safe: true` — analysis-only; safe to
    fan out across multiple impl blocks.
  - `cost_class: cheap` — single-file syntactic walk.
- Per-atom dependencies: RA-B1, RA-T1, RX-G1.

**Gates.**
- `bbox_artifact_install(kind="agent",
  source="examples/agents/refactor/rust-impl-partition-graph.json")`
  succeeds; agent appears in `bro_agent_list(kind="agent")`.
- **Deterministic discovery gate**: with `include_vectors=false`
  (keyword-only search; deterministic in CI),
  `bro_agent_search("partition rust impl methods",
  include_vectors=false)` returns this atom; alternative form
  with `exclude_anti_pattern_matches=false` against an
  anti-pattern query (e.g., "decide partition for rust impl")
  returns the atom with `matched_anti_patterns` non-empty.
- `bro_agent_dispatch(agent="rust-impl-partition-graph",
  args={project_dir, source_file: "tests/fixtures/refactor_agents/
  rust_impl_partition_graph/basic.rs", impl_name: "impl Foo"})`:
  - JSONSchema validation: missing `impl_name` rejects before
    spawn.
  - Dispatch succeeds; agent runs grounding sequence; emits
    `bbox_note(kind="done")` with a one-line summary.
  - Response includes `methods`, `fields`, `edges` shapes;
    advisory schema validation in v1.
- **Deterministic filter-overlay check** (Codex round-1 of this
  doc review):
  - `bro_agent_describe(agent="rust-impl-partition-graph")`
    response's `merged_filters` contains
    `mcp__blackbox__bbox_forget` in disallow.
  - Adversarial live-agent test is NOT a v1 gate (LLM behavior
    is non-deterministic); it can exist as an opt-in test under
    `tests/adversarial/`.

**Follow-ups.**
- Add a clustering-atom companion (`rust-impl-cluster-suggester`)
  later; chainable_after this graph atom.

---

### Phase RA-A2 — `rust-public-api-guard` (precondition)

**Scope.** Wraps `rust_public_api_guard` (RX-G2). Used as a
precondition by other atoms (notably `rust-error-migrate`,
future atoms touching pub surfaces).

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-public-api-guard`".

**Components.**
- Manifest at `examples/agents/refactor/rust-public-api-guard.json`:
  - Standard fields per the design doc.
  - `cost_class: normal` (Codex round-1 of design review:
    `cheap` was optimistic for directory scans).
  - Inputs include `proposed_changes` (optional list of plan-step
    refs) for context.
- Per-atom dependencies: RA-B1, RA-T1, RX-G2.

**Gates.**
- Install + search + dispatch as RA-A1.
- Dispatch returns `advisory_severity` field with `info` |
  `caution` | `breaking` classification.
- Modifying a `pub fn` signature flags `breaking` in the response.

**Follow-ups.**
- Future `rust-public-api-guard-fast` variant for file-scoped uses
  with `cost_class: cheap` (design doc open question).

---

### Phase RA-A3 — `rust-test-island-extract`

**Scope.** Peel inline `#[cfg(test)] mod tests` blocks into
sibling `src/tests/*.rs` files (NOT crate-level `tests/`).

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-test-island-extract`".

**Components.**
- Manifest at
  `examples/agents/refactor/rust-test-island-extract.json`.
- Per-atom dependencies: RA-B1, RA-T1. NO new Rust plan-kind
  dependencies — uses `extract_rust_items` already in production,
  plus `add_rust_mod_decl` for `mod tests;`.
- Inputs include `source_file_or_dir`, `target_dir`
  (default `"src/tests"`), `apply: bool`.
- Output extension: `extracted_test_files: [{source, target,
  test_count, refs_preserved}]`.
- Prompt template constrains the atom to:
  1. Confirm `#[cfg(test)]` blocks exist via
     `bbox_refactor_status` (item_kinds=["mod_item"], filter for
     cfg(test) attribute).
  2. Plan `extract_rust_items` to `src/tests/<basename>.rs` for
     each block.
  3. Plan `add_rust_mod_decl` for `#[cfg(test)] mod tests;` in
     the source file's parent module declaration list.
  4. Run `cargo test --bin blackboxd` (or operator-specified
     binary) as the validation command.

**Gates.**
- Install + search.
- Dispatch on a file with one `#[cfg(test)] mod tests` block:
  produces a plan moving the block to `src/tests/<basename>.rs`
  with a `mod tests;` declaration in the source.
- `super::*` references in the test block survive — gate fixture
  has tests that import via `super::*` and the atom verifies they
  resolve post-move (since sibling modules share parent scope).
- Refuse fixture: test block referencing items via
  `super::super::*` (would not survive sibling move) → atom
  blocks with reason in `block_reason`.

**Follow-ups.**
- Per-test-file split (group tests by tested module) — not in v1.

---

### Phase RA-A4 — `rust-state-extract`

**Scope.** Pull a field cluster into a separate struct; wire as
delegate; rewrite source-side accesses conservatively.

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-state-extract`".

**Components.**
- Manifest at `examples/agents/refactor/rust-state-extract.json`.
- Per-atom dependencies: RA-B1, RA-T1, RX-A1b (Copy whitelist),
  RX-A2 (plan-only FIXME markers for the blocked-plan refusal
  path), RX-S1, RX-S2a, RX-S2b, RX-W1a, RX-W1b.
- Inputs: standard set per the design doc.
- Composition: `chainable_after: ["rust-impl-partition-graph"]`
  — operators often look at the graph first to decide which
  fields cluster.
- Prompt template explicitly invokes:
  - `bbox_refactor_plan(kind="extract_rust_items",
    item_kinds=["struct_item"])` to create the target struct
    declaration.
  - `bbox_refactor_plan(kind="move_rust_struct_fields",
    deep_analysis=true)` to move fields.
  - `bbox_refactor_plan(kind="add_rust_delegate_field")` and
    `bbox_refactor_plan(kind="update_rust_callers",
    emit_applied_markers=<from_inputs>)` for the conservative
    rewrite + borrow-promotion markers.
  - Composes through `bbox_refactor_run` with cargo check +
    compile-fix.

**Gates.**
- Install + search + dispatch.
- Dispatch on a fixture struct + apply=true: state extracted,
  delegate wired, source compiles after cargo check + compile-fix
  resolves residual sites.
- Dispatch with `acknowledge_repr` unset on a `#[repr(C)]`
  source: atom blocks; `block_reason` references the repr.
- Operator-authority: atom does NOT default
  `acknowledge_repr: true` even on user prompt suggesting it
  should "just work" — input must be explicit.
- `unrewriteable_accessors` non-empty case: atom emits the
  count in the done-note and proceeds (the conservative rewrite
  refused those sites; compile-fix may or may not handle them).

**Follow-ups.**
- Auto-clustering atom variant that picks the field cluster
  rather than requiring operator input.

---

### Phase RA-A5 — `rust-trait-from-impl`

**Scope.** Lift a method subset into a trait + impl. Hard refuse
`migrate_call_sites: true` when `object_safety_report.dyn_compatible
: false`.

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-trait-from-impl`".

**Components.**
- Manifest at
  `examples/agents/refactor/rust-trait-from-impl.json`.
- Per-atom dependencies: RA-B1, RA-T1, RX-T1, RX-M1.
- Inputs: per the design doc, including `migrate_call_sites`,
  `call_site_replacement`.
- **Hard refusal logic in prompt template**: after planning
  `extract_rust_trait` with `deep_analysis=true`, if
  `object_safety_report.dyn_compatible: false` AND
  `inputs.migrate_call_sites: true`, the atom emits
  `bbox_note(kind="blocked")` with a concrete diagnostic and
  returns `status: "blocked"` WITHOUT attempting the migration.
- Output extension: `trait_file`, `methods_lifted`,
  `object_safe`, `call_site_warnings`, `migration_skipped`,
  `call_sites_migrated`.

**Gates.**
- Install + search + dispatch.
- Fixture: struct with `&self` methods → trait extracted, impl
  block created, cargo check passes.
- Fixture: method takes `Self` by value, `dyn_compatible:
  false`, `migrate_call_sites=true` → atom blocks. Hard refusal
  in `block_reason`.
- Fixture: method takes `Self` by value but
  `migrate_call_sites=false` → atom proceeds with the lift; the
  caller is expected to use `Box<dyn>` / generic-param-trait
  binding at the call sites.

**Follow-ups.**
- LSP-backed `rust_ra_extract_trait` variant later (design doc
  follow-up).

---

### Phase RA-A6 — `rust-error-migrate`

**Scope.** Rewrite a module's error type. The atom runs the
`rust_public_api_guard` PLAN KIND (RX-G2) as a PREFLIGHT plan
BEFORE its mutating `bbox_refactor_run`, NOT as a step inside
that run, and NOT as a separate atom dispatch. v1 composition is
intra-prompt preflight-then-run, not atom-to-atom dispatch.

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-error-migrate`".

**Components.**
- Manifest at
  `examples/agents/refactor/rust-error-migrate.json`.
- Per-atom dependencies: RA-B1, RA-T1, RX-E1, RX-G2 (plan kind,
  not atom), RX-C1. RA-A2 (the public-api-guard atom) is NOT a
  build-time dep; operators may still chain RA-A2 → RA-A6 in a
  workflow, but RA-A6 does not require RA-A2 to be installed.
- Inputs: standard set including `error_mapping`,
  `acknowledge_public_api_change`.
- **Prompt template — preflight then mutating run** (Codex
  round-2 sharpening: keep the guard OUTSIDE the mutating run
  so no `bbox_refactor_run(confirm=true)` ever starts on a
  blocked path):
  1. Ground via `bbox_code_symbols` / `bbox_refactor_status`.
  2. Preview the proposed `rewrite_rust_error_type` plan
     (e.g., via `bbox_refactor_plan` without confirm) to obtain
     a plan-ref or proposed-change summary.
  3. Run `bbox_refactor_plan(kind="rust_public_api_guard",
     source=…, proposed_changes=[<preview plan-ref>])` as a
     PREFLIGHT plan, OUTSIDE any `bbox_refactor_run`. Inspect
     `advisory_severity`.
  4. If `advisory_severity: breaking` AND
     `inputs.acknowledge_public_api_change != true`: emit
     `bbox_note(kind="blocked")` with the guard's findings;
     return `status: "blocked"`. **No `bbox_refactor_run` is
     created on this path.** The atom must NOT default the
     acknowledge flag.
  5. Only if the preflight allows: create a `bbox_refactor_run`
     containing the `rewrite_rust_error_type` plan, the
     `continue_for_repair` cargo-check command capturing rustc
     JSON, the `rust_compile_fix_round` repair plan, and a
     final `cargo check` validation step.
- The guard plan deliberately runs as a preflight, not as a
  step inside the mutating run. This keeps the transaction
  story clean: the only `confirm=true` run executes when the
  guard already cleared, so the repair-transaction invariant
  (RX-F2b) governs only the mutating sequence.
- v2 atom-to-atom dispatch (per
  `design/orchestration/agents/agent-system-impl.md:608`) is NOT used here; v1
  composition is intra-prompt sequencing of preflight + run.

**Gates.**
- Install + search + dispatch.
- Fixture: pure-internal error type at
  `tests/fixtures/refactor_agents/rust_error_migrate/internal.rs`
  in a tiny temp Cargo crate → migrates cleanly.
- Fixture: `pub` error type at
  `tests/fixtures/refactor_agents/rust_error_migrate/public.rs`
  without `acknowledge_public_api_change` → atom blocks; guard
  step's `advisory_severity: breaking` carried in
  `block_reason`.
- Fixture: `?`-site incompatibility at
  `tests/fixtures/refactor_agents/rust_error_migrate/qmark.rs`
  → compile-fix consumes; if leftovers, atom returns non-zero
  `fixme_count.plan_only` and a done-note.
- Operator-authority: dispatch with no
  `acknowledge_public_api_change` in inputs against a public
  error type → blocks. Dispatch with
  `acknowledge_public_api_change: true` → proceeds.

**Follow-ups.**
- v2 atom-to-atom composition is tracked in
  `design/orchestration/agents/agent-system-impl.md:608` but not required by RA-A6.

---

### Phase RA-A7 — `rust-split-god-impl` (headline atom)

**Scope.** Carve a multi-domain `impl T` block into per-domain
modules. The headline atom; composes RX-A1a-d / RX-A2 / RX-R2 /
RX-C1.

**Realizes.** `design/refactor-agents.md` catalog entry
"`rust-split-god-impl`".

**Components.**
- Manifest at
  `examples/agents/refactor/rust-split-god-impl.json`.
- Per-atom dependencies: RA-B1, RA-T1, RX-A1a, RX-A1b, RX-A1c,
  RX-A1d, RX-A2, **RX-R2 (REQUIRED, not optional)**, RX-C1. Also:
  existing `extract_rust_impl_methods`, `add_rust_router_to_sum`,
  `add_rust_mod_decl`, `rewrite_rust_item_visibility`,
  `rust_organize_imports`.
- Codex round-1 of this doc review: shipping the headline atom as
  `IndexedHints`-only would weaken the core safety story.
  `rust-split-god-impl` requires `resolved_callbacks` from RX-R2
  to safely classify cross-partition method calls; an unresolved
  callback in the wrong partition is exactly the silent-miscompile
  case the atom must refuse. A separate degraded
  `rust-split-god-impl-syntax-only` variant could ship later if
  there's demand for an RA-unavailable mode; v1 is RA-required.
- Inputs: `source_file`, `impl_name`, `partition: {<domain>:
  [<method-names>]}`, `allow_cross_partition_delegation`,
  `apply`.
- Prompt template:
  1. Ground via `bbox_code_symbols` + `bbox_refactor_status`.
  2. For each partition: plan
     `bbox_refactor_plan(kind="extract_rust_impl_methods",
     deep_analysis=true)`.
  3. **Chain `rust_ra_classify_callbacks`** (mandatory, not
     optional — Codex round-2 fix for the RX-R2 requirement).
     If rust-analyzer is unavailable, the underlying plan kind
     returns `error.lsp_unavailable` per RX-V3; the atom emits
     `bbox_note(kind="blocked")` with the fail-closed reason
     and returns `status: "blocked"`.
  4. Inspect `resolved_callbacks` (now populated): any call
     resolving to a method in a DIFFERENT partition's set is a
     cross-partition call. If any cross-partition call exists
     AND `allow_cross_partition_delegation: false`, block.
  5. Otherwise compose a `bbox_refactor_run` with per-partition
     extract + `add_rust_router_to_sum` + `add_rust_mod_decl` +
     `rewrite_rust_item_visibility` (widening to `pub(crate)`
     for cross-partition callees when delegation allowed) +
     `rust_organize_imports` + cargo check + compile-fix +
     cargo test.

**Gates.**
- Install + search + dispatch.
- Fixture: small impl with two clean partitions at
  `tests/fixtures/refactor_agents/rust_split_god_impl/clean_two_partition/`
  (a tiny temp Cargo crate) → splits, cargo check passes, cargo
  test passes.
- Fixture: impl with cross-partition method calls at
  `.../cross_partition_calls/`,
  `allow_cross_partition_delegation: false` → blocks with concrete
  diagnostic listing the cross-partition calls (sourced from
  `resolved_callbacks`, not raw `unresolved_callbacks`).
- Fixture: impl with cross-partition calls,
  `allow_cross_partition_delegation: true` → proceeds, widening
  visibility to `pub(crate)` on the called methods.
- Cargo test failure → run rolls back atomically per the repair
  transaction invariant.
- **Production-scale smoke (plan-only)**: dispatch with `apply:
  false` against the actual `src/main.rs::BlackboxServer` impl
  with one realistic partition (e.g., agents-tools as one
  domain). The plan generates without panicking; saved plan_path
  inspectable. Apply against the live repo is NOT a gate — too
  brittle (Codex round-1 of this doc review). A separate
  end-to-end test runs the same dispatch in a disposable
  `git worktree`-style copy of the repo and asserts the apply
  + cargo workflow succeeds; that test lives under
  `tests/end_to_end_refactor_agents/` and runs in a marked
  category (slow / live).

**Follow-ups.**
- Composition with `rust-impl-partition-graph` (operator chains).
- Auto-partition-suggester atom (separate; not in initial
  catalog).

---

## Optional cross-language appendix

### Phase RA-X1 — `java-extract-cohesive-class` (appendix atom)

**Scope.** Cross-language proof-of-shape atom per the design doc's
appendix. Demonstrates the manifest contract carries over to Java
without modification.

**Realizes.** `design/refactor-agents.md` "Cross-language reference
(appendix)".

**Components.**
- Manifest at
  `examples/agents/refactor/java-extract-cohesive-class.json`.
- Per-atom dependencies: RA-B2 (Java persona), RA-T1, existing
  `extract_java_class` with deep_analysis (already in production
  per `sm-refactor-java`).
- Validation command: `mvn test` / `./gradlew test` per the
  operator's project setup.

**Gates.**
- Install + search.
- Dispatch on a Java fixture: clean cluster extraction round-trips
  with the deep_analysis report.
- Same outputs.schema shape as Rust atoms (proves cross-language
  symmetry).

**Follow-ups.**
- This phase is optional. If the team's polyglot stories don't
  materialize, defer indefinitely without affecting Rust atoms.

---

## Cross-cutting documentation phases

### Phase RA-V1 — Composition hand-wiring documentation

**Scope.** Document the v1 composition story: `chainable_after`,
`parallel_safe`, `fan_out_aggregator` are signals to workflow
authors; the manifest fields do not autoload at runtime per
`design/orchestration/agents/agent-system-impl.md` §608. Workflows hand-wire atom
chains.

**Realizes.** `design/refactor-agents.md` "Composition — aspirational
in v1"; "Composition patterns".

**Components.**
- `sm-refactor` entry describing the three canonical composition
  patterns (sequential chain, pre-flight + execute, analysis +
  decision + execute, fan-out across languages) with workflow
  fragments for each.
- Reference workflow JSONs at
  `examples/agents/refactor/workflows/`:
  - `state-extract-then-split.json` — RA-A4 → RA-A7.
  - `error-migrate-with-guard.json` — RA-A2 → RA-A6.
  - `partition-graph-then-split.json` — RA-A1 → RA-A7.

**Gates.**
- `sm-refactor` entry exists.
- Reference workflows install via the existing workflow
  artifact-install path and dispatch round-trips end-to-end.

**Follow-ups.**
- v2 composition primitive (`bro_agent_compose` per
  `design/orchestration/agents/agent-system-impl.md` §608) — not in this skeleton.

---

### Phase RA-V2 — Distillation hook documentation

**Scope.** Document the path for `provenance: distilled` atoms.
This skeleton specifies hand-authored atoms only; the distiller is
out of scope.

**Realizes.** `design/refactor-agents.md` "Provenance — distillation
path (acknowledged, out of scope)".

**Components.**
- `sm-refactor` entry stating:
  - Initial catalog is hand-authored.
  - Future distilled atoms enter the catalog via
    `bbox_artifact_install` with `provenance: distilled` and
    agentic-corpus edges back to source sessions.
  - Distiller implementation tracked separately
    (badgey-flavor; references `design/badgey.md` /
    `design/badgey-impl.md` once those phases reach a relevant
    stage).
- `AgentProvenance::Distilled` variant
  (`src/orchestration/agents/types.rs:170`) is already in the
  schema; this phase only documents the workflow.

**Gates.**
- `sm-refactor` entry exists referencing distillation as a future
  workflow.

**Follow-ups.**
- Distiller implementation (separate impl plan, out of scope).

---

## Eval coverage

Eval phases ensure atoms continue to behave per their contracts as
the underlying plan kinds and infrastructure evolve.

### Phase RA-E1 — Per-atom dispatch + behavior-smoke eval

**Scope.** Per-atom eval covering: install → search → dispatch
→ result-shape round trip, PLUS a minimal behavior smoke
asserting the atom orchestrates the right plan-kind sequence and
emits the right notes. These are integration-level tests
alongside the existing `eval/agents/` infrastructure
(`discovery-queries.json`, `cuing-scenarios.json` precedent).

**Realizes.** Operational hygiene; required for v1 atoms to
remain trustworthy as plan kinds evolve.

**Components.**
- `eval/agents/refactor/discovery-queries.json` extending the
  existing discovery eval with refactor-atom queries (per
  atom's `when_to_use`).
- `eval/agents/refactor/dispatch-scenarios.json` — per-atom
  dispatch with a fixture input + response-shape assertion.
- `eval/agents/refactor/behavior-smoke.json` — per-atom minimal
  behavior smoke. NOT exhaustive semantic testing (that's RX
  per-plan-kind territory). The smoke covers, per atom:
  - **Grounding**: did the atom call `bbox_code_symbols` /
    `bbox_refactor_status` before planning?
  - **Plan sequence**: did the atom invoke the expected plan
    kinds in order? (e.g., RA-A4 must invoke `extract_rust_items`
    then `move_rust_struct_fields` then
    `add_rust_delegate_field` then `update_rust_callers`.)
  - **Block reachability**: dispatch with a precondition-
    violating input → atom emits
    `bbox_note(kind="blocked")` with the expected
    `block_reason` substring.
  - **Done note**: clean dispatch → atom emits
    `bbox_note(kind="done")` with the agent's expected
    summary shape.
- **Implementation — deterministic template/sequence simulation
  via recording adapter** (Codex round-2/3 fix): the
  behavior-smoke harness registers a recording/fake
  `AgentDispatchAdapter` (per
  `src/orchestration/agents/adapter.rs:71`) for the atoms under
  test. The adapter receives the manifest + args and SIMULATES
  the prompt template's intended tool-call sequence (parsing
  the template's `bbox_refactor_*` markers in order), without
  actually invoking a live LLM. This makes the test
  deterministic and CI-stable. Codex round-3 caveat: the
  adapter does NOT observe real LLM tool calls — it interprets
  the template's intended sequence. So this gate confirms the
  TEMPLATE-INTENDED orchestration, not the LIVE-LLM
  orchestration. Live `bbox_messages` introspection on a real
  dispatched session is the secondary integration check,
  marked slow/live; the recording adapter is the v1 default.
- Codex round-1 of this doc review: do not defer all behavior
  to RX — atom-wrapper orchestration contract is the agent
  layer's responsibility.
- Eval-runner integration via `eval/check.rs`.
- **Embedding-readiness gate** (Codex round-1): discovery eval
  runs in one of two modes, never both:
  - **Keyword-only mode** (default in CI): pass
    `include_vectors=false` so search is deterministic.
  - **Vector-ready mode** (gated, requires waiting until
    `bro_agent_list` shows `embedding_pending=false` for all
    installed atoms): `include_vectors=true`; tolerates ranking
    fluctuation.

**Gates.**
- Eval runner picks up new entries; round-trip passes for each
  atom.
- Discovery eval (keyword-only mode): each atom returns for at
  least one query matching its `when_to_use`.
- Discovery eval anti-pattern phrasing (Codex round-1):
  with `exclude_anti_pattern_matches=true`, anti-pattern queries
  do NOT return the atom; with `false`, the atom returns with
  `matched_anti_patterns` non-empty. Do NOT gate on "ranks
  top-3" — small catalogs make that flaky.
- Behavior smoke: each atom passes the grounding + plan-
  sequence + block-reachability + done-note checks for at least
  one fixture.

**Follow-ups.**
- Vector-ready discovery eval moves out of opt-in once the
  embedding pipeline stabilizes.
- Behavior eval deepens per atom over time; v1 ships minimal
  coverage to catch contract regressions.

---

### Phase RA-Z1 — Supersession + replacement policy

**Scope.** Define the refactor-atom-specific supersession policy
on top of the existing `bbox_artifact_supersede` mechanics.

**Realizes.** Operational hygiene; not a design-doc section per
se, but required so atom version bumps don't corrupt callers.

**Components.**
- Each new atom version bump uses
  `bbox_artifact_supersede(kind="agent", name="<atom>",
  superseded_by="<atom>@v<N+1>")`.
- Refactor-atom-specific rule: superseded atoms are hidden from
  `bro_agent_search` AND `bro_agent_list` unless
  `include_superseded=true`. Default agent-system behavior
  already filters list; this phase verifies search also filters
  (regression-gate it).
- Re-embed cadence: each new atom version triggers an embedding
  refresh in the `agent_manifest` bucket. Documented; the
  existing agent-system pipeline handles the mechanics. The
  refactor-atom phase calls it out so operators know the cost
  per version bump.
- **Pinned-dispatch behavior** (Codex round-2 grounding:
  `bro_agent_dispatch` rejects inactive records at
  `src/tools/agents.rs:522` with `agent '...' is not active
  (superseded or deactivated)`):
  - `bro_agent_get(name="<atom>@v<N>")` and
    `bro_agent_describe(...)` resolve superseded versions
    (read paths permit `include_superseded=true`).
  - `bro_agent_dispatch(agent="<atom>@v<N>")` of a superseded
    version is **rejected** by the existing dispatch path.
    Operators who need to dispatch an older version must
    explicitly un-supersede it (the existing
    `bbox_artifact_supersede` mechanics support this), or pin
    to a version that's still active.
  - This phase does NOT add an `allow_superseded` flag to
    dispatch; doing so would weaken the "active version is
    canonical" invariant. If pinned-superseded dispatch
    becomes a real operator need, it's a separate v2 design.
- `sm-refactor` documents the supersession rules per refactor
  atom.

**Gates.**
- Install atom v2 superseding v1: v1 disappears from
  default-search and default-list.
- `include_superseded=true` surfaces v1.
- Pinned `bro_agent_get(name="<atom>@v1")` and
  `bro_agent_describe(...)` resolve v1 (read paths permit
  superseded targets).
- Pinned `bro_agent_dispatch(agent="<atom>@v1")` REJECTS with
  the existing `agent '...' is not active (superseded or
  deactivated)` error from `src/tools/agents.rs:522`. No
  `allow_superseded` flag in v1.
- Embedding refresh fires on install; `embedding_pending` toggles
  through the expected lifecycle.
- `sm-refactor` entry exists.

**Follow-ups.**
- Removal/cleanup policy for ancient superseded versions —
  separate decision; v1 keeps history indefinitely.

---

### Phase RA-D1 — `sm-refactor` cross-catalog index

**Scope.** Consolidate the per-atom `sm-refactor` entries into a
catalog-style index that lists every shipped refactor atom with
one-line summary and last-version pointer. This is the single
discoverable entry point operators query for "what refactor atoms
are available."

**Realizes.** Operational hygiene; the doc-cross-linking
follow-up flagged in Codex round-1 of this doc review.

**Components.**
- `sm-refactor` entry titled "Refactor atom catalog"
  enumerating every shipped atom:
  - Atom name, current version.
  - One-line description (sourced from manifest).
  - `when_to_use` first bullet.
  - `cost_class`.
  - Required RX-phase dependencies.
  - Cross-link to the atom's manifest path in
    `examples/agents/refactor/`.
- Updated as each new atom phase lands (gate-checked: a new atom
  doesn't land without its catalog entry).
- Cross-language note: the catalog covers Rust atoms; the Java
  appendix atom (RA-X1) gets its own entry once it ships.

**Gates.**
- `bbox_knowledge(query="refactor atom catalog")` returns the
  index entry.
- Catalog enumerates every atom currently in
  `examples/agents/refactor/`.
- **CI-enforced completeness check** (Codex round-2 fix:
  manual checklist is too weak): a CI step compares the set of
  manifests under `examples/agents/refactor/*.json` against the
  catalog entry's atom list; mismatch fails the build. A
  follow-up `tools/refactor-atom-catalog-gen` script can
  auto-regenerate the catalog from manifests, removing the
  drift risk entirely.

**Follow-ups.**
- Auto-generation from the manifest set via the script above,
  so atom landings update the catalog mechanically.

---

## Phase dependency DAG

```
Substrate:
  RA-B1 (rust persona) ────► RA-S1 (manifest lint) ──┐
                                                     │
  RA-T1 (template + base outputs schema) ────────────┤
                                                     ▼
                              every Rust atom phase (RA-A1..A7)

  RA-B2 (java persona) ──► RA-X1 (java appendix atom)
  (RA-B2 only required when Java atoms ship; RA-S1's
   recognized-personas list grows when RA-B2 lands)

Build-time deps from RX-:
  RX-F1a, RX-F1b, RX-F2a, RX-F2b, RX-A1a/b/c/d, RX-A2, RX-C1,
  RX-V1, RX-V2, RX-V3
                         │
                         ▼
                         every atom phase

Per-atom additional RX- deps:
  RA-A1 (impl-partition-graph) ◄─ RX-G1
  RA-A2 (public-api-guard)     ◄─ RX-G2
  RA-A3 (test-island-extract)  ◄─ (no new Rust kinds; uses extract_rust_items + add_rust_mod_decl)
  RA-A4 (state-extract)        ◄─ RX-A1b, RX-A2, RX-S1, RX-S2a, RX-S2b, RX-W1a, RX-W1b
  RA-A5 (trait-from-impl)      ◄─ RX-T1, RX-M1
  RA-A6 (error-migrate)        ◄─ RX-E1, RX-G2, RX-C1   (intra-run composition, NOT RA-A2 dep)
  RA-A7 (split-god-impl)       ◄─ RX-A1a/b/c/d, RX-A2, RX-R2 (REQUIRED), RX-C1
  RA-X1 (java appendix)        ◄─ RA-B2 (and existing Java kinds)

Cross-cutting docs:
  RA-V1 (composition docs) — independent
  RA-V2 (distillation docs) — independent

Operational hygiene:
  RA-Z1 (supersession policy) — gates atom-version bumps; independent of atom landing order
  RA-D1 (sm-refactor catalog index) — gated per atom landing

Eval:
  RA-E1 — depends on all RA-A* atoms it covers; behavior smoke
          for each. Eval entries land as their atoms land;
          partial eval coverage is OK.

Recommended landing order:
  RA-B1 → RA-T1 → RA-S1 → RA-A1 → RA-A2 → RA-A3 → RA-A4
       → RA-A5 → RA-A6 → RA-A7 → RA-V1 → RA-V2 → RA-Z1
       → RA-D1 → RA-E1 → [optional] RA-B2 → RA-X1
```

## Non-goals (this skeleton)

- v2 dispatch-side output-schema validation per
  `design/refactor-agents.md` "Refusal — prompt-discipline in v1".
- Runner-side enforcement of the command allowlist (v2 path per
  RX-V2).
- Atom-internal composition of other atoms (today's composition
  is operator/workflow-driven; future
  `design/orchestration/agents/agent-system-impl.md` §608 work enables atom-to-atom
  chains).
- Auto-distillation of atoms from corpus evidence (RA-V2 doc only;
  distiller implementation separate).
- A clustering atom paired with RA-A1 — separate phase.
- An auto-partition-suggester atom for RA-A7 — separate phase.
- A `rust-public-api-guard-fast` cheap variant for RA-A2 — design-
  doc open question.
- LSP-backed `rust_ra_extract_trait` for RA-A5 — design-doc
  follow-up.

## Known follow-ups across phases

- Cross-link entries in `sm-refactor` as each atom lands. The
  cross-language `sm-refactor` carries the atom catalog summary;
  per-language `sm-refactor-rust` / `sm-refactor-java` cover the
  plan kinds the atoms compose.
- Author-tooling: a `tools/refactor-atom-fill` helper so atom
  manifests stay in sync with the shared template (RA-T1
  follow-up).
- Shipping policy for brofiles (design-doc open question 1) —
  decision documented in `sm-refactor` once made.
- Embedding bucket re-embed cadence: every atom version bump
  triggers a re-embed in the `agent_manifest` bucket (existing
  agent-system behavior; called out here so the operator
  understands the cost).

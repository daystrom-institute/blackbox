---
title: "Refactor Agents - atomic, harness-agnostic refactor patterns"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
  - atoms
date: 2026-05-10
revision: "rev 2 - applies codex-gpt55 review convergence"
status: "design proposal, pure design (no implementation phasing)"
brief: "Defines reusable refactor atoms as named, contract-bearing wrappers around mechanical refactor workflows."
---

# Refactor Agents — atomic, harness-agnostic refactor patterns

Related: `design/orchestration/agents/agent-system.md`, `design/orchestration/agents/agent-system-impl.md`,
`design/refactor-tools/rust/refactor-rust-expansion.md`, `sm-refactor`, `sm-refactor-rust`,
`sm-refactor-java`, `sm-agentic-opening-sequence`

## Problem

The blackbox refactor surface — `bbox_refactor_status`, `bbox_refactor_plan`,
`bbox_refactor_apply`, `bbox_refactor_run`, plus the AST locators
`bbox_code_symbols`, `bbox_code_node_describe`, `bbox_code_query` — provides
mechanical refactor primitives with parse-validation, hash checks, and
transactional rollback. What it does not provide is a **named,
discoverable, contract-bearing surface** for common refactor patterns.

A user who wants to "split a god-impl into per-domain modules" today must:

1. Recall the right plan kinds exist.
2. Decide partition boundaries.
3. Ground via `bbox_code_symbols` + `bbox_refactor_status`.
4. Author one plan per partition.
5. Author `add_rust_router_to_sum` + `add_rust_mod_decl` per partition.
6. Author visibility rewrites where partition surface requires them.
7. Compose into `bbox_refactor_run` with `cargo check` + repair.
8. Read FIXMEs from the deep_analysis report, decide whether to apply.
9. Emit a `bbox_note(kind="done")` summary.

This is a discipline, not a tool. Every operator re-derives the same
flow. Mistakes are silent: skipping step 3 means the plan refuses with a
name mismatch; skipping step 8 means applying through unresolved
captures; skipping step 9 means the orchestrator's inbox doesn't surface
the result.

The bbox agent infrastructure (`design/orchestration/agents/agent-system.md`, implemented at
`src/orchestration/agents/` and exposed via `bro_agent_list / search /
describe / dispatch / get`) is the right surface for this. An agent is:

> a brofile with a manifest, where the manifest is "Claude frontmatter
> on steroids" — same role (selection cuing) but rich enough to power
> semantic discovery, composition contracts, provenance, and
> cross-provider dispatch.

This design specifies the **atomic refactor agent** as a class of
`kind="agent"` JSON artifacts:

- Each atom encapsulates **one refactor pattern**.
- Each atom's filter overlay restricts its tool surface (with the
  enforcement caveats documented below).
- Each atom declares its inputs (JSONSchema validated before dispatch)
  and outputs (consumable by chained agents or by an orchestrator,
  though validation is advisory in v1).
- Each atom is `cross-provider` — any caller (Claude, Codex, Gemini,
  OpenCode) can dispatch it; the brofile decides which provider
  executes.
- Each atom is `discoverable` via `bro_agent_search` so callers find it
  by intent rather than name.

Refactor agents are **not a new infrastructure layer**. They are JSON
manifests installed via `bbox_artifact_install(kind="agent", source=…)`
and discovered through the existing registry. The novelty is the design
pattern, not the substrate.

## What "atomic" means — honest scope

Codex round-1 reviewed an earlier draft that overclaimed mechanical
enforcement. The corrected position:

### Selection cuing, not enforcement

The `description`, `when_to_use`, and `anti_patterns` fields constrain
the atom to one refactor pattern **as documentation and as ranked search
signal** (`src/orchestration/agents/registry.rs:347`). They are not a
hard contract — the manifest schema doesn't prove "one intent" and the
search pass uses positive/anti-pattern scoring, not refusal.

What this gets us: a user searching "split a Rust god-impl" finds one
atom — not seven variants of "Rust refactor stuff." That's a real
product win even though it's prose discipline, not enforcement.

### Tool surface — filter merge semantics

`MergedFilters::merge` at `src/orchestration/agents/adapter.rs:135` is
**additive with deny-wins on exact overlap**. The atom's `filter_overlay`
can:

- ADD allow patterns on top of the brofile's allows.
- ADD disallow patterns on top of the brofile's disallows. Deny-wins
  means if a pattern is in both lists after merge, deny prevails.

The overlay CANNOT narrow the brofile's allows. If the underlying
brofile allows `mcp__blackbox__bbox_*`, the atom can still call every
`bbox_*` tool unless the overlay explicitly DENIES each one it wants
to forbid.

**Consequence**: refactor atoms cannot rely on a permissive shared
brofile + restrictive per-atom overlay. **The brofile must already be
narrow.** This design specifies a dedicated `rust-refactor-persona`
brofile (and `java-refactor-persona` for Java atoms) whose allow list
contains only the refactor-and-grounding tool set. Atoms then narrow
further via deny additions, never via allow restrictions.

The brofile prerequisite is a hard precondition for the atomic-agent
contract. Without it, an atom's filter_overlay is documentation, not
mechanism.

### Contract — inputs enforced, outputs advisory

`bro_agent_dispatch` compiles and validates `inputs.schema` before
spawning (`src/tools/agents.rs:656`). Malformed args fail before any
tool fires. This part is mechanical.

`outputs.schema` is **advisory** in v1 (`design/orchestration/agents/agent-system.md:432`).
The dispatch path returns `{session, task_id, resolved_brofile,
merged_filters, agentLabel}` (`src/tools/agents.rs:781`); it does not
validate that the agent actually returned the declared output shape.

Atoms therefore have an asymmetric contract: callers can trust the
input schema rejected bad args, but cannot assume the LLM-emitted
output conforms to its declared schema. Composing workflows that
consume atom outputs must defensively validate or accept best-effort
shape.

### Composition — aspirational in v1

`composition.chainable_after`, `parallel_safe`, `fan_out_aggregator`
exist in the manifest struct (`src/orchestration/agents/types.rs:113`),
but per `design/orchestration/agents/agent-system-impl.md:608` there is no
`bro_agent_compose` consumer in v1. Workflows hand-wire composition
through the existing workflow engine; the manifest fields are signal
for those workflow authors, not autoload by the runtime.

This is fine — composing refactor atoms through `bbox_refactor_run` plus
a small workflow is the v1 pattern. The manifest fields document the
intent.

### Refusal — prompt-discipline in v1

The atom's prompt template specifies `bbox_note(kind="blocked")` and
return status when preconditions don't hold. The dispatch path does
not parse the agent's textual output for `status="blocked"` or
otherwise enforce the refusal. Refusal is **prompt-discipline**: the
LLM follows the protocol or doesn't.

What makes this acceptable for the common case: refactor primitives
themselves are transactional. An atom that "refuses badly" (continues
despite preconditions) typically hits `bbox_refactor_apply`'s
hash/parse/scope checks or `cargo check` failure inside the run, and
either errors out or rolls back via the repair transaction invariant.

**Honest worst case** (Codex round-4 hit): the runner accepts arbitrary
`command` values in `bbox_refactor_run` command steps. A misbehaving
atom that composes a mutating command WITHOUT declaring `touches` can
mutate files outside the snapshot/rollback set. The runner does not
filter commands by allowlist today. Also: a prompt-discipline failure
that produces a successful-but-wrong refactor (cargo check passes; the
result is wrong) won't trigger rollback because there's no failure
signal.

Two compensating controls in v1:

1. **Brofile denial of `Write`, `Edit`, `Bash`.** The atom literally
   cannot shell out or write files outside the refactor surface. The
   only mutation path is `bbox_refactor_run` command steps.
2. **Command-allowlist invariant** (`design/refactor-tools/rust/refactor-rust-expansion.md`
   "Cross-Surface Invariants"). Atom-dispatched runs are
   prompt-disciplined to use only the `cargo check / test / clippy /
   fmt / build` allowlist for command steps, and mutating commands
   outside that list must declare `touches`. This is prompt-level in
   v1; the runner cannot distinguish atom-dispatched runs from operator
   runs.

v2 paths:

- Dispatch-side output validation against `outputs.schema`, surfacing
  structured refusal explicitly.
- Runner-side awareness of atom-dispatched runs, enforcing the
  command allowlist at the run level instead of via prompt-discipline.

## Non-Goals

- Architectural decision-making. Atoms execute mechanical patterns with
  crisp preconditions; they do not decide *whether* a refactor is
  worthwhile.
- General-purpose "refactor anything" agents. An agent whose charter is
  "do whatever the user wants" is just an executor with a long prompt.
- Inventing a new dispatch mechanism. Refactor atoms use the
  direct-dispatch path (`src/tools/agents.rs::bro_agent_dispatch` →
  `bro_exec`). No `dispatch_adapter`.
- Replacing the existing system memories (`sm-refactor-*`). System
  memories document the manual workflow; atoms automate the common
  shapes. Both coexist.
- Auto-distillation of new atoms from corpus evidence
  (`provenance: distilled` path). Acknowledged but out of scope; this
  design specifies hand-authored atoms only.
- Output-schema enforcement at dispatch time. v2.

## Prerequisite: the narrow refactor brofiles

Two brofiles must exist before any refactor atom is dispatchable. Their
purpose is to bound the maximum tool surface refactor atoms can reach,
since atom filter overlays can only add denies.

### `rust-refactor-persona`

```jsonc
{
  "name": "rust-refactor-persona",
  "provider": "claude",                  // or operator's choice
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
  "lens": "You execute mechanical Rust refactor patterns through the bbox refactor primitives. Ground every operation via bbox_code_symbols and bbox_refactor_status before planning. Plan with deep_analysis=true. Compose primitives through bbox_refactor_run with a cargo check command step gated by repair. Emit bbox_note(kind=\"done\") with a one-line acceptance summary on completion. Refuse cleanly with bbox_note(kind=\"blocked\") and a concrete diagnostic when preconditions don't hold; never loop, never broaden charter."
}
```

Notes:

- `Bash` is **disallowed**. Cargo runs through `bbox_refactor_run`
  command steps, where the runner manages snapshot/rollback. This
  was a Codex round-1 hit; rev 1 had Bash in the allow list.
- `Write` and `Edit` are disallowed. Refactor primitives do their own
  atomic writes. An atom that needs to edit a file outside the
  refactor surface is not atomic; it's a general executor.
- `bbox_learn` / `bbox_remember` / `bbox_decide` / `bbox_forget` /
  `bbox_render` are disallowed. Atoms emit findings via `bbox_note`,
  not durable knowledge. Codex round-1 caught the missing `bbox_learn`
  / `bbox_remember` in the rev-1 disallow list.
- `bro_*` is fully disallowed (recursion guard).

### `java-refactor-persona`

Identical shape with Java-flavored lens prose. Allow list adds nothing
beyond what the Rust persona has — the refactor primitives are
language-agnostic at the MCP surface; the per-plan-kind language
routing happens inside the daemon.

## Manifest contract for refactor atoms

```jsonc
{
  "kind": "agent",
  "name": "<atom-name>",
  "version": 1,
  "manifest": {
    "description": "<single sentence describing the refactor pattern>",
    "when_to_use": [
      "<concrete situation>"
    ],
    "anti_patterns": [
      "<situation where the atom should refuse / a different atom applies>"
    ],

    "brofile_ref": "rust-refactor-persona",

    "filter_overlay": {
      "allow": [],
      "disallow": [
        // per-atom narrowing — extra denies on top of brofile.
        // example: an atom that doesn't need refactor_run can deny it
        // to make the surface obvious to readers, though the brofile
        // already permits it.
      ]
    },

    "inputs": {
      "schema": { /* atom-specific JSONSchema; deep_analysis: true REQUIRED for any plan-kind invocation */ },
      "prompt_template": "<the agentic-opening-sequence protocol; see below>"
    },

    "outputs": {
      "schema": { /* atom-specific; advisory in v1 */ },
      "evidence_density": "high"
    },

    "composition": {
      "chainable_after": [],
      "parallel_safe": false,
      "fan_out_aggregator": null
    },

    "cost_class": "normal",

    "provenance": {
      "kind": "hand_authored",
      "author": "<author>",
      "created_at": "<ISO-8601>"
    }
  }
}
```

### Shared `prompt_template` shape — the grounding sequence baked in

Every refactor atom encodes the agentic-opening-sequence
(`sm-agentic-opening-sequence`) plus the language refactor system
memory's protocol:

```
You are <atom-name>. Charter: <one-line pattern>.

Protocol:

1. Ground the target structurally:
   bbox_code_symbols(project_dir="{{project_dir}}", query="{{symbol_or_file_hint}}",
                     languages=["{{language}}"], item_kinds=[...])
   bbox_refactor_status(file="{{source_file}}", project_dir="{{project_dir}}", ...)
   Copy exact `name` and `kind` values from the response. Do not name-match
   from the user's prompt — re-derive from the structural inventory.

2. Plan with deep_analysis=true (REQUIRED for atoms):
   bbox_refactor_plan(kind="<the-pattern>", deep_analysis=true, ...)
   Inspect the response for: captured_self_fields, unresolved_callbacks,
   resolved_callbacks, remaining_source_accessors, inherited_generics,
   call_site_warnings — whichever this pattern surfaces.

3. Decide:
   - If unresolved captures/dependencies exceed atom-specific thresholds
     declared in inputs (default: any unresolved external call → block),
     save the plan via output_path, emit bbox_note(kind="blocked",
     body=<concrete diagnostic with line numbers + plan_path>) and return
     status="blocked".
   - Otherwise proceed.

4. Apply (if inputs.apply == true) or return plan-only:
   bbox_refactor_run(confirm=true, steps=[
     <plan steps>,
     {"op":"command","command":"cargo","args":["check","--message-format=json"],
      "capture":"rustc_json","on_failure":"continue_for_repair"},
     {"op":"plan","kind":"rust_compile_fix_round","diagnostics_ref":"last"},
     {"op":"command","command":"cargo","args":["check"],"required":true},
     {"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"required":true}
   ])

5. Emit done note:
   bbox_note(kind="done", body=<one-line summary: files-touched count,
     fixme count, plan_path if blocked, cargo result>).

Strict refusal rules (prompt-discipline; not enforced at dispatch):
- Never call any tool outside your filter_overlay.
- Never invent symbol names from the user prompt — re-derive from
  bbox_code_symbols / bbox_refactor_status.
- Never apply when status=blocked.
- Never proceed past a cargo check failure (the runner rolls back
  per the repair transaction invariant; do not retry without
  resolving the underlying diagnostics).
- Never edit files outside the planned set (Write/Edit are denied
  at the brofile anyway, but the discipline is documented).

Inputs:
{{args}}
```

### Shared `outputs.schema` shape

Every refactor atom declares (advisory in v1):

```jsonc
{
  "type": "object",
  "required": ["status"],
  "properties": {
    "status": { "enum": ["planned", "applied", "blocked", "errored"] },
    "plan_path": { "type": "string", "description": "saved plan path; required when status=blocked or planned" },
    "files_touched": { "type": "array", "items": { "type": "string" } },
    "fixme_count": {
      "type": "object",
      "properties": {
        "plan_only": { "type": "integer", "minimum": 0 },
        "warning": { "type": "integer", "minimum": 0 }
      }
    },
    "deep_analysis_summary": {
      "type": "object",
      "description": "atom-specific subset of the deep_analysis report",
      "additionalProperties": true
    },
    "cargo_result": {
      "type": "object",
      "properties": {
        "command": { "type": "string" },
        "exit_code": { "type": "integer" },
        "summary": { "type": "string" },
        "rolled_back": { "type": "boolean" }
      }
    },
    "block_reason": { "type": "string" },
    "done_note_id": { "type": "string" }
  }
}
```

`fixme_count` is split (`plan_only` vs `warning`) to reflect the
two-prefix FIXME grammar in `design/refactor-tools/rust/refactor-rust-expansion.md`.

## Catalog — initial Rust atoms

Seven atoms. Each one JSON artifact installable via
`bbox_artifact_install`. Per-atom catalogs below list only the
distinguishing fields; the shared manifest contract above applies.

### `rust-split-god-impl`

Carve a multi-domain `impl T` block into per-domain modules.

- **When**: a single impl block holds 20+ methods that cleanly partition
  by domain (e.g., `BlackboxServer` → search / knowledge / orchestration
  / refactor).
- **Anti**: do not use to decide partitions — chain
  `rust-impl-partition-graph` first (separate atom, the
  `rust_impl_partition_analysis` plan kind front-end). Do not use when
  partitions share mutable state on `self` — chain `rust-state-extract`
  first.
- **Inputs**: `source_file`, `impl_name`, `partition: {<domain>:
  [<method-names>]}`, `allow_cross_partition_delegation: bool`, `apply:
  bool`.
- **Output extension**: `partitions: [{domain, target_file,
  moved_methods, captured_self_fields, unresolved_callbacks,
  resolved_callbacks, fixme_count, repair_steps_applied}]`.
- **Composition**: `chainable_after: ["rust-impl-partition-graph",
  "rust-state-extract"]`, `parallel_safe: false`.

### `rust-state-extract`

Pull a cluster of `self.<field>` reads into a separate struct, wire as
delegate, rewrite source-side accesses conservatively.

- **When**: 5+ fields read/write together as a logical cluster;
  operator wants separate testing or to break a circular dependency.
- **Anti**: complex initialization order (the atom wires constructors
  mechanically, doesn't strategize); `#[repr(C)]` / `#[repr(packed)]`
  unless `acknowledge_repr: true`.
- **Inputs**: `source_file`, `source_struct`, `target_struct_name`,
  `target_module_path`, `field_names`, `delegate_field`,
  `acknowledge_repr: bool`, `apply: bool`.
- **Output extension**: `delegate_field, accessors_generated,
  remaining_source_accessors, unrewriteable_accessors, borrow_promotions`.
- **Note**: per the Rust expansion doc, `update_rust_callers` is
  conservative. Many sites will appear in `unrewriteable_accessors`
  and the compile-fix round may handle some; the rest are operator
  follow-ups.

### `rust-trait-from-impl`

Lift a method subset into a `trait`, add `impl Trait for Struct`.

- **When**: a struct has a public method surface callers should depend
  on by trait, not concretion. Operator wants mockability.
- **Anti**: methods take `Self` by value or return `Self` — trait
  cannot be used as `dyn Trait` (atom flags via `object_safety_report`).
  Methods call other inherent methods on `self` not in lift set —
  atom refuses cleanly.
- **Inputs**: `source_file`, `source_struct`, `trait_name`, `target_file`,
  `method_names`, `migrate_call_sites: bool`, `call_site_replacement:
  string` (e.g., `"Arc<dyn MyTrait>"`), `apply: bool`.
- **Hard refusal**: when `migrate_call_sites: true` AND
  `object_safety_report.dyn_compatible: false`, refuse. Cannot migrate
  to `dyn Trait` if the trait isn't object-safe. Codex round-1 hit:
  rev 1 had this as soft refusal; now hard.
- **Output extension**: `trait_file, methods_lifted, object_safe,
  call_site_warnings, migration_skipped, call_sites_migrated`.

### `rust-error-migrate`

Rewrite a module's error type. Narrowed scope (Codex round-1): signature
+ literal-construction rewrites only. `?`-site conversions handled by
`rust_compile_fix_round`.

- **When**: a module's errors are not actionable for callers, or over-
  specified.
- **Anti**: error sites use `downcast` / `downcast_ref` (atom refuses);
  error type is exposed via public API — atom blocks via
  `rust-public-api-guard` precondition and refuses unless the operator
  passed `acknowledge_public_api_change: true` in inputs.
- **Inputs**: `source_file_or_module`, `from_type`, `to_type`,
  `error_mapping: {old_construction_form: new_construction_form}`,
  `acknowledge_public_api_change: bool` (operator-authority; see
  invariant below), `apply: bool`.
- **Output extension**: `signatures_rewritten,
  construction_sites_rewritten, question_mark_sites: [{site,
  classification}], repair_round_diagnostics`.

**Operator-authority invariant** (applies here and to any atom that
exposes `acknowledge_repr` or `acknowledge_public_api_change`): the
atom passes these flags through from operator-supplied inputs to the
underlying plan kind. The atom MUST NOT default them, MUST NOT infer
them from context, and MUST NOT silently set them after a refusal.
See `design/refactor-tools/rust/refactor-rust-expansion.md` "Operator-authority opt-outs"
for the full statement.

### `rust-test-island-extract`

Peel inline `#[cfg(test)] mod tests` blocks into sibling **`src/tests/*.rs`**
files, declared via `mod tests;` in `lib.rs` / `main.rs`. Sibling
modules in the same crate, NOT crate-level integration tests.

- **When**: a file's test block exceeds 200 lines and pollutes the
  source file; a test island is itself monolithic (e.g.,
  `src/tests.rs`, 5614 lines).
- **Anti**: do not use when test blocks reference items via `super::*`
  in ways that don't survive the move (atom reports and refuses).
- **Inputs**: `source_file_or_dir`, `target_dir: "src/tests"`, `apply:
  bool`.
- **Output extension**: `extracted_test_files: [{source, target,
  test_count, refs_preserved}]`.

Codex round-1 hit: rev 1 said crate-level `tests/`. That makes tests
into integration tests and loses private access. Rev 2 corrects to
`src/tests/*.rs` siblings.

### `rust-impl-partition-graph`

Produce the partition graph for an impl block. Front-end for the
`rust_impl_partition_analysis` plan kind. Clustering is a SEPARATE
atom or workflow step (Codex round-2: graph and clustering separate).

- **When**: about to run `rust-split-god-impl` and want to see method-
  field-call structure first; humans want to inspect before committing
  to a partition.
- **Anti**: do not use as a clustering algorithm; this atom returns
  the graph, not partitions.
- **Inputs**: `source_file`, `impl_name`.
- **Output extension**: full graph from
  `rust_impl_partition_analysis` (methods / fields / edges).
- **Cost class**: `cheap`. Analysis-only; no plan or apply.

### `rust-public-api-guard`

Front-end for the `rust_public_api_guard` plan kind. Precondition atom
chained before any pattern that touches `pub` items.

- **When**: about to run a refactor that may modify public surfaces
  (visibility rewrites, re-export inlining, error-type changes on
  pub functions).
- **Anti**: do not use as a permission grant; this atom reports the
  delta, doesn't decide whether the change is safe.
- **Inputs**: `source` (file or dir), `proposed_changes: [<plan-step
  refs>]`.
- **Output extension**: `public_items_touched, public_api_delta_summary,
  crate_root_re_exports_affected, advisory_severity`.
- **Cost class**: `normal`. (Codex round-4: `cheap` was optimistic
  when `source` is a directory or crate-root re-export scan; the
  manifest's `AgentCostClass` is a single value, not conditional, so
  picking the worst-case class is the honest call. File-scoped uses
  could be a separate `rust-public-api-guard-fast` variant later.)

## Cross-language reference (appendix)

Codex round-2 push: the lone Java atom in the rev-1 catalog crowded the
Rust list. Moved to an appendix here as a proof-of-shape demonstration.

### `java-extract-cohesive-class` (appendix only)

Java parallel showing the cross-language symmetry: same manifest
shape, same shared template, different brofile (`java-refactor-persona`),
different plan kind (`extract_java_class` with `deep_analysis: true`),
different validation command (`mvn test` / `./gradlew test`).

A polyglot interface-extraction workflow can fan-out across this atom
and `rust-trait-from-impl` with a `merge-refactor-results` aggregator
(once that aggregator exists; aspirational in v1 per
`design/orchestration/agents/agent-system-impl.md:608`).

## Composition patterns

Three canonical shapes. All composition is hand-wired through workflows
in v1.

### Sequential chain

`rust-state-extract → rust-split-god-impl`

State extraction lands first so the god-impl partition references a
clean state struct. Workflow wires output (`delegate_field`,
`target_struct_name`) → input partition plan.

### Pre-flight + execute

`rust-public-api-guard → rust-error-migrate (apply=true)`

Guard reports the public-API delta; the migration atom proceeds only
when delta is clean OR operator explicitly acknowledges.

### Analysis + decision + execute

`rust-impl-partition-graph → human/clustering-atom decides → rust-split-god-impl`

The graph atom produces structural facts; a decision step (operator
review, or a `cost_class: expensive` clustering atom — not in this
initial catalog) chooses partitions; the splitter executes.

### Fan-out across languages

Workflows can fan-out across `rust-trait-from-impl` and
`java-extract-cohesive-class` (the appendix) with a
`merge-refactor-results` aggregator. Each atom runs in its own language;
the aggregator collects results.

`parallel_safe: false` on individual atoms doesn't prevent fan-out — they
parallelize across **different files / projects**, not against each
other.

## Discovery

`bro_agent_search("split a rust impl into per-domain modules")` returns
ranked agents by:

- Semantic similarity over the `agent_manifest` embedding bucket
  (description + when_to_use + anti_patterns).
- Anti-pattern penalty: a query matching an atom's anti-patterns drops
  the score sharply.
- Optional `cost_class` filter.

Selection-by-cue is the same pattern Claude's native `.claude/agents/*.md`
files use; refactor atoms get it for every provider through the bbox
MCP surface, without bbox having to read or ship in Claude's file
format (`design/orchestration/agents/agent-system.md` §2.1).

## Provenance — distillation path (acknowledged, out of scope)

Initial catalog is `provenance: hand_authored`. The agent system's
distillation hook (`design/orchestration/agents/agent-system.md` §1.1) leaves space for a
badgey-flavor distiller to mine the corpus for recurring refactor task
shapes and propose new atoms with `provenance: distilled` plus
agentic-corpus edges back to source sessions. The `AgentProvenance::Distilled`
variant (`src/orchestration/agents/types.rs:170`) is in the schema.

This design does not specify the distiller.

## Open Design Questions

1. **Brofile shipping policy.** Whether `rust-refactor-persona` and
   `java-refactor-persona` ship with the daemon (consistency across
   installs) or require operators to author per-install (avoids biasing
   prose). The narrow-allow constraint is the load-bearing part;
   the lens prose is somewhat opinionated.
2. **Atom-level deep_analysis enforcement.** `inputs.schema` declares
   `deep_analysis: true` as a constant required field. Whether to also
   add a dispatch-side check that the atom actually invoked the plan
   with that arg (parsing tool-call traces) is open. v1 trusts the
   prompt template; v2 could verify.
3. **Output-schema validation.** v1 is advisory. v2 could validate the
   final agent emission against `outputs.schema` and surface structured
   refusal. Open question whether the cost (LLM-emitted JSON drift
   creating churn) is worth the contract enforcement payoff.
4. **`unrewriteable_accessors` follow-up.** When
   `rust-state-extract` leaves many unrewriteable accessors, should
   the atom emit a follow-up suggestion (e.g., "dispatch
   rust-compile-fix-round-atom on the diagnostics"), or stay strict
   and let the orchestrator decide?
5. **Atom version bumps for new plan kinds.** When a Rust plan kind
   gains a new opt-in field (e.g., `replacement_kind` on
   `migrate_rust_type_usages`), do existing atoms bump to v2 or stay
   on v1? In-place supersede is cleaner versioning; variant atoms are
   cleaner discovery. Likely answer: in-place when new field is
   opt-in with safe default; variant when semantics change.
6. **Pin scope for atoms.** Atoms emitting `bbox_pin` create state that
   survives their session. Project-scoped pins enable compositional
   continuity ("the partition the operator chose"); session-scoped
   pins are more honest about atom statelessness. Open policy
   question.

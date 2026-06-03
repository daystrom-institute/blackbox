---
title: "Atom System - Implementation Plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - orchestration
  - atoms
tags:
  - atoms
  - refactor-tools
date: 2026-05-13
status: "implemented core runtime; implementation plan retained for audit trail"
brief: "Phased build plan for the atom runtime, registry, invocation handles, resolver kinds, and workflow bindings."
---

# Atom System — Implementation Plan

Companion to: [Atom System](atom-system.md)

Implementation note: phases covering `ArtifactKind::Atom`, atom registry/read
tools, invocation handles, profile/workflow/deterministic/adapter execution,
ownership/delegation, output-shape validation, effect-limit enforcement, and
workflow `atom_bindings` have landed. Remaining work is catalog conversion and
deprecation cleanup for legacy `bro_agent_*` defaults, not the atom runtime.

Related:
- [Agent System](../agents/agent-system.md) - predecessor implementation shape.
- [Workflow Engine](../../../docs/workflows.md) - current workflow actor/subworkflow model.
- [Supervision Impl](../supervision/supervision-impl.md) - advisor/oracle policy substrate.
  state.
- [Phase Decomposer](../phase-decomposer/phase-decomposer.md) and
  [Phase Decomposer Impl](../phase-decomposer/phase-decomposer-impl.md) - workflow-level
  parallelism, decomposition, recomposition, and mediation.
- [Refactor Compound Runs](../../refactor-tools/refactor-compound-runs.md) - transactional refactor
  runner.
- `sm-refactor` and `sm-refactor-rust` - current bbox refactor tool runbooks.

## Implementation Thesis

Do not treat this as a public migration. The current `agent` surface is private
scaffolding, not an adopted user-facing contract. Optimize for the correct atom
architecture:

```text
ArtifactKind::Atom
  -> standalone atom manifests
  -> typed atom refs
  -> atom registry + atom_* tools
  -> policy-gated atom invocation
  -> AtomBinding workflow integration
  -> closed deterministic/adapter resolver set
```

The existing `src/orchestration/agents/*` code is donor material: registry
projection, manifest validation, brofile resolution, embeddings, selection
cuing, and dispatch logic. Reuse pieces when useful, but do not preserve
`kind="agent"`, `bro_agent_*`, or legacy agent-shaped refactor contracts as
public concepts.

Workflow JSON should stay pure workflow. Workflow-backed atoms are standalone
atom artifacts whose `implementation.kind="workflow"` points at a workflow ref.
Only workflows intended to be invoked as reusable capabilities need atom
wrappers; private subworkflows can remain plain workflow artifacts.

## Refactor Tool Strategy

Use refactor tools for bounded mechanical work, not ontology decisions.

Good uses:

- `bbox_code_symbols` for locating symbol clusters
- `bbox_refactor_status` for exact item names and refactorability
- `extract_rust_impl_methods` or `extract_rust_items_to_submodule` when
  splitting large modules
- `rust_lsp_rename` for binding-aware symbol renames
- `move_file` plus explicit module-declaration edits for module moves
- `bbox_refactor_run` for transactional command validation
- `rust_compile_fix_round` after mechanical extraction

Do not use them for:

- broad text replacement of `agent` with `atom`
- deciding schema shape
- replacing workflow engine design with generated code
- hiding validation gaps behind compile-fix rounds

Default code search to indexed mode, falling back to live only if the index is
stale or truncated:

```text
bbox_code_symbols(project_dir=..., query="AgentManifest", mode="indexed")
bbox_refactor_status(file="src/orchestration/agents/types.rs", item_kinds=["struct","enum"])
```

Plan-kind names such as `extract_rust_impl_methods`, `replace_text`,
`rust_lsp_rename`, `move_file`, and `rust_compile_fix_round` are not standalone
tools. Invoke them with `bbox_refactor_plan(...)` + `bbox_refactor_apply(...)`,
or as `{"op":"plan","kind":...}` steps inside `bbox_refactor_run`.

Every mutating Rust phase should end with:

```json
{"op":"command","command":"cargo","args":["fmt"],"on_failure":"required"}
{"op":"command","command":"cargo","args":["test","--bin","blackboxd"],"on_failure":"required"}
```

Avoid broad `touches:["src"]` declarations for formatting commands. Omit
`touches` or name exact files.

## Phase DAG

```text
Phase 0 ─▶ Phase 1 ─▶ Phase 2 ─▶ Phase 3 ─▶ Phase 4 ─▶ Phase 5
```

Phase 0 inventories private agent scaffolding. Phase 1 creates atom storage,
refs, schema, and validators. Phase 2 adds the registry and read/search tools
and rewrites shipped refactor atoms. Phase 3 implements invocation, handles,
ownership, and trace summaries. Phase 4 integrates workflows through
`AtomBinding`. Phase 5 adds deterministic and adapter resolver variants from a
closed in-daemon registry.

## Phase 0: Inventory And Baseline

**Prerequisites:** none.

**What gets built:** no product behavior; this is grounding.

0.1 **Symbol inventory.** Record the private scaffolding that will either move
or be replaced:

- `src/artifacts.rs`
- `src/orchestration/agents/types.rs`
- `src/orchestration/agents/registry.rs`
- `src/orchestration/agents/validate.rs`
- `src/tools/agents.rs`
- `src/tools/bro_params.rs`
- `src/tools/orchestrate.rs`
- `src/workflow/schema.rs`
- `src/workflow/mod.rs`
- `src/workflow/engine.rs`
- `src/orchestration/mcp.rs`
- `src/tool_docs.rs`
- `system-defaults/mcp-surfaces/*`
- `schema/workflow.schema.json`
- `system-defaults/atoms/refactor/*.json` (canonical refactor atom path)
- `src/system_memory/refactor*.md`

0.2 **Current behavior snapshot.** Use `bbox_code_symbols` and
`bbox_refactor_status` to record the current `bro_agent_*` behavior and tests.
These become rewrite targets, not compatibility tests.

```text
bbox_refactor_status(
  file="src/tools/agents.rs",
  item_kinds=["impl_method"],
  project_dir="/home/invidious/repos/transcript-search"
)
```

**Deliverable:** an issue note or scratch doc listing current symbol/test names.

**Refactor tools:** `bbox_code_symbols`, `bbox_refactor_status`.

## Phase 1: Atom Schema, Refs, And Storage

**Prerequisites:** Phase 0.

**What gets built:**

1.1 **Artifact kind.** Add `ArtifactKind::Atom` and canonical artifact storage.
Do not preserve `ArtifactKind::Agent` as a public artifact kind.

Canonical atom envelope:

```json
{
  "_contract": "atom/v1",
  "kind": "atom",
  "name": "rust-refactor-plan",
  "version": 1,
  "manifest": {}
}
```

1.2 **Typed refs.** Add first-class typed refs:

- `atom:name@v1` - pinned atom ref
- `atom:name@latest` - explicit floating atom ref
- `workflow:name@v1`, `brofile:name@v1`, `packet:name@v1`, `team:name@v1`
  where a typed cross-artifact ref is needed

Reject bare names in stored artifacts. Bare names may be accepted only at
operator/tool boundaries and resolved before persistence.

1.3 **Manifest structs.** Add canonical atom types:

- `AtomArtifact`
- `AtomManifest`
- `AtomImplementation`
- `AtomEffects`
- `AtomComposition`
- `AtomSupervisionPolicy`
- `AtomTracePolicy`
- `AtomRunHandle`
- `AtomInvocation`
- `AtomBinding`

1.4 **Implementation union.** Model `implementation` as a closed tagged enum:

```json
{ "kind": "profile", "brofile_ref": "brofile:rust-refactor-persona@v1" }
{ "kind": "workflow", "workflow_ref": "workflow:research-review@v1" }
{ "kind": "deterministic", "runner": "refactor-plan-validate" }
{ "kind": "adapter", "adapter_name": "badgey" }
```

Do not allow `implementation.kind="bro"`. A bro is the runtime worker produced
by a profile-backed invocation.

1.5 **Subcontracts.** Add optional typed subcontract overlays:

```json
{
  "_contract": "atom/v1",
  "subcontract": "refactor/v1"
}
```

Subcontracts drive extra validation. Tags and category remain discovery
metadata only.

1.6 **Input/output schema.** Use JSON Schema 2020-12 under `inputs.schema` and
`outputs.schema`.

1.7 **Effects and composition.** Enforce v1 closed shapes:

```json
{
  "effects": {
    "writes_files": false,
    "dispatches_runs": 0,
    "max_depth": 0,
    "uses_network": false
  },
  "composition": {
    "may_invoke_atoms": { "kind": "none" }
  }
}
```

`writes_files` may later be `{"scoped":["path/glob"]}`.
`uses_network` may be `{"gated_by":"packet:network-policy@v1"}`.
`may_invoke_atoms` is a tagged enum: `none`, `any`, or `allowed`.

Do not add `parallel_safe`, `chainable_after`, `chainable_before`, or
`fan_out_aggregator` to atom v1. Phase-decomposer owns parallelism at workflow
level.

**Deliverable:** atom artifacts parse and validate; invalid refs, unknown
implementation kinds, invalid subcontract values, and invalid effects fail.

**Tests:**

- `ArtifactKind::Atom` install/list/supersede
- `AtomRef` pinned/latest parsing and stored bare-name rejection
- manifest validation for all implementation variants
- JSON Schema presence and malformed-schema rejection
- `refactor/v1` subcontract validation dispatch

**Refactor tools:** hand edits first. Use `rust_lsp_rename` only for isolated
helpers after tests exist.

## Phase 2: Atom Registry, Read Tools, And Catalog Rewrite

**Prerequisites:** Phase 1.

**What gets built:**

2.1 **Atom registry.** Add `AtomRegistry` as the read-only projection over
`ArtifactKind::Atom`. It should support:

- list active atoms
- get by `AtomRef`
- describe contract and implementation summary
- search selection cues and embeddings
- include superseded on request

2.2 **Embedding bucket.** Use `atom_manifest` as the canonical embedding bucket.
Do not document `agent_manifest` as public behavior.

2.3 **Read tools.** Add:

- `atom_list`
- `atom_get`
- `atom_describe`
- `atom_search`

Remove `bro_agent_*` from caller-facing docs and surfaces. Temporary private
shims are acceptable only inside the branch while tests are being rewritten.

2.4 **Refactor atom rewrite.** Rewrite shipped refactor artifacts directly:

- `kind: "atom"`
- `_contract: "atom/v1"`
- `subcontract: "refactor/v1"`
- `implementation.kind: "profile"`
- `implementation.brofile_ref` bound to the narrow refactor persona
- explicit `effects`, `trace`, `supervision`, and `composition`

2.5 **RA-S1 lint.** Update refactor lint to dispatch on
`subcontract="refactor/v1"`, not category/tags.

**Deliverable:** shipped refactor atoms are discoverable through
`atom_search` and readable through `atom_get`.

**Tests:**

- every shipped atom appears in `atom_search`
- `subcontract="refactor/v1"` requires narrow brofile refs
- `acknowledge_*` inputs still cannot have defaults
- tool-doc coverage includes all read tools
- MCP surface examples expose atom reads on readonly surfaces

**Refactor tools:** JSON/doc edits are mostly manual. Use `replace_text` only
for exact reviewed examples. Use `bbox_refactor_run` command steps for catalog
and system-memory test gates.

## Phase 3: Invocation, Policy, Handles, And Trace

**Prerequisites:** Phase 2.

**What gets built:**

3.1 **Invoke/status/resume tools.**

- `atom_invoke`
- `atom_status`
- `atom_resume`
- `atom_delegate`

3.2 **Invocation identity.** Generate an atom-level `invocation_id` at
`atom_invoke` time. Do not overload provider `session_id`, task id, or workflow
arc id.

3.3 **Run handle store.** Persist `AtomInvocation` records with:

- `invocation_id`
- `atom_ref`
- `parent_invocation_id`
- owners
- handle kind
- status
- started/ended timestamps
- trace summary

Handle variants:

```text
Profile { provider, session_id, project_dir, task_id }
Workflow { workflow_ref, arc_id, root_task_id }
Deterministic { runner }
Adapter { adapter_name, adapter_handle }
```

3.4 **Ownership.** Creator becomes initial owner:

- parent invocation id for nested invocations
- `operator:<account>` for human/operator calls

Only owners can call `atom_status` and `atom_resume`. `atom_delegate` grants
another invocation ownership. v1 does not need revocation.

3.5 **Budget and policy.** Before invocation, compute:

```text
effective_limit = min(contract, binding, invocation, parent_remaining)
```

Enforce:

- `effects.dispatches_runs`
- `effects.max_depth`
- `effects.writes_files`
- `effects.uses_network`
- `composition.may_invoke_atoms`

Budget exhaustion and depth exhaustion return structured errors.

3.6 **Tool classification.** Replace prefix-only recursion logic with a
classification table:

- default allowed: `atom_list`, `atom_get`, `atom_describe`, `atom_search`
- ownership gated: `atom_status`, `atom_resume`, `atom_delegate`
- policy gated: `atom_invoke`
- default denied inside spawned bros: raw `bro_exec`, `bro_resume`,
  `bro_cancel`, and equivalent raw orchestration

3.7 **Trace summary.** `atom_status` returns a normalized summary envelope:

```json
{
  "invocation_id": "01...",
  "parent_invocation_id": null,
  "atom_ref": "atom:research-review@v1",
  "implementation_kind": "workflow",
  "state": "succeeded",
  "started_at": "2026-05-13T00:00:00Z",
  "ended_at": "2026-05-13T00:01:00Z",
  "input_digest": "sha256:...",
  "output_digest": "sha256:...",
  "output_shape": {
    "valid": true,
    "schema_ref": "outputs.schema",
    "errors": []
  },
  "summary": "short result or failure summary",
  "decision_points": [],
  "children": [],
  "effects_observed": {
    "writes_files": false,
    "dispatches_runs": 2,
    "uses_network": true,
    "violations": []
  },
  "cost": {
    "input_tokens": 0,
    "output_tokens": 0,
    "dispatched_runs": 2,
    "wall_time_ms": 0
  },
  "artifacts": [],
  "errors": []
}
```

States: `queued`, `running`, `succeeded`, `failed`, `cancelled`, `timed_out`,
`expired`.

**Deliverable:** profile-backed `atom_invoke` returns an owned handle;
`atom_status` returns normalized status; `atom_resume` works only on owned
resumable handles.

**Tests:**

- profile invocation creates `AtomInvocation`
- non-owner status/resume is forbidden
- deterministic handles return `not_resumable`
- budget/depth exhaustion fails before dispatch
- raw `bro_exec` remains denied inside spawned bros
- trace summary validates against expected shape

**Refactor tools:** hand edits. Use `bbox_refactor_run` for `cargo fmt`,
targeted atom tests, and full `cargo test --bin blackboxd`.

## Phase 4: Workflow Integration Through AtomBinding

**Prerequisites:** Phase 3.

**What gets built:**

4.1 **AtomBinding.** Replace `ActorSpec` as the reusable local workflow binding:

```rust
pub struct AtomBinding {
    pub id: String,
    pub atom_ref: AtomRef,
    pub durable: bool,
    pub compaction_anchor: bool,
    pub capability_requires: Vec<Capability>,
    pub limits: Option<BindingLimits>,
    pub supervision_override: Option<SupervisionPolicy>,
    pub trace_override: Option<TracePolicy>,
    pub portal: Option<PortalBindingOverride>,
}
```

Keep `durable` and `compaction_anchor`; they are already meaningful workflow
concepts. Split provider capability requirements from effects. `filesystem` and
`network` are atom effects, not provider capabilities.

4.2 **Node references.** Nodes reference bindings by id, replacing the current
`actor: String` relationship. Node control-flow fields stay on `NodeSpec`:

- prompt
- gate/gate_mode
- mode
- retry
- late_inject
- wait/wait_for
- foreach/matrix
- failure handling
- transitions

4.3 **Subworkflow relationship.** Workflows may keep private inline
`subworkflow` and private `subworkflow_ref` for implementation. Reusable
capabilities are invoked through atom refs. A workflow-backed atom is a
standalone atom whose implementation points at `workflow:<name>@vN`.

4.4 **Engine escape hatch validation.** Workflow install/compile validation
rejects atom-backed workflows or binding-local overrides that violate the
target atom's effects:

- raw `bro_exec`, `bro_resume`, `bro_cancel`, or raw workflow orchestration when
  dispatch budget is zero or exhausted
- file-writing hook ops when `effects.writes_files=false`
- networked hook ops when `effects.uses_network=false`
- child atom invocations outside `composition.may_invoke_atoms`

4.5 **Phase-decomposer alignment.** Do not solve parallelism in atom schema.
Phase-decomposer handles parallel work via:

- workflow DAGs
- `foreach` batching
- supervised subworkflows
- collected outcomes
- recomposition council
- mediation/remediation loops

Atom v1 intentionally has no `parallel_safe` contract.

**Deliverable:** workflows can invoke atoms through local `AtomBinding`
definitions, and workflow-backed atoms remain standalone atom artifacts.

**Tests:**

- workflow binding resolves atom ref
- durable binding reuses/resumes as current durable actors do
- binding limits tighten atom contract limits
- private subworkflow still works
- atom wrapper can invoke workflow by `workflow_ref`
- escape-hatch violations fail validation

**Refactor tools:** use `bbox_refactor_status` on workflow schema/engine files
before editing. Use extract/move helpers only after schema tests exist.

## Phase 5: Deterministic And Adapter Resolver Kinds

**Prerequisites:** Phase 3. Phase 4 for workflow use.

**What gets built:**

5.1 **Closed deterministic runner registry.** Add in-daemon deterministic
runners needed by current artifacts:

- refactor validation/planning runners
- packet evaluators
- lint/format/test gates where they are better modeled as atoms than raw hooks

No third-party runner registration in v1.

5.2 **Closed adapter registry.** Add in-daemon adapter dispatch for known custom
systems such as Badgey. Adapters declare:

- accepted input schema
- output schema
- handle shape
- resumability
- observed effects reporting

No third-party adapter registration in v1.

5.3 **Status/resume behavior.**

- deterministic atoms usually return terminal handles and `not_resumable`
- adapters declare whether `atom_resume` is supported
- all variants emit the same trace summary envelope

**Deliverable:** all four implementation variants parse, validate, invoke where
registered, and report normalized status.

**Tests:**

- unknown runner rejected at install/validation time
- unknown adapter rejected at install/validation time
- deterministic terminal status
- adapter handle/status path
- trace summary parity across implementation kinds

**Refactor tools:** mostly hand edits. Use `rust_compile_fix_round` only after
the variant skeletons compile and tests expose mechanical fallout.

## Deferred Past V1

- Third-party deterministic runner registration.
- Third-party adapter registration.
- Path-aware parallel write conflict semantics.
- Workflow fanout/aggregator schema beyond existing `foreach.collect`.
- Atom-to-atom dataflow type checking beyond JSON Schema structural validation.
- Stable v1.1 trace summary serialization after real traces exist.
- Catalog-sprawl mitigation if standalone workflow-backed atoms become noisy.

## Suggested First Code Slice

The smallest useful slice is:

1. Add `ArtifactKind::Atom`, `AtomRef`, `AtomArtifact`, and `AtomManifest`.
2. Add validators for `_contract`, `subcontract`, implementation union,
   effects, composition, and JSON Schema fields.
3. Add read-only `AtomRegistry`.
4. Add `atom_list`, `atom_get`, `atom_describe`, and `atom_search`.
5. Rewrite one shipped refactor artifact to `kind="atom"` with
   `subcontract="refactor/v1"`.
6. Rewrite the matching tests away from `bro_agent_*`.

This gives the project the correct public vocabulary before invocation,
workflow integration, or deterministic/adapters add runtime complexity.

## Risks

- **Internal rename churn before behavior.** Rename public concepts now, but
  move Rust modules only when atom tests make the behavior stable.
- **Raw orchestration leakage.** `atom_invoke` must be policy gated before it is
  exposed to spawned bros.
- **Workflow hook bypass.** Daemon-side hooks can bypass provider-facing MCP
  filters; validate workflow hook ops against atom effects.
- **Refactor safety drift.** `subcontract="refactor/v1"` must retain narrow
  brofile binding and acknowledgement-input linting.
- **Overclaiming parallel safety.** Keep conflict semantics at workflow /
  phase-decomposer level until concrete scoped-write needs exist.
- **Catalog sprawl.** Standalone atoms are correct, but docs should distinguish
  reusable capabilities from private subworkflows.

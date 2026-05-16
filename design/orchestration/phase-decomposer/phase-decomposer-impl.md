---
title: "Phase Decomposer - Implementation Plan"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - orchestration
  - phase-decomposer
date: 2026-05-10
status: "implemented; live no-edit smoke coverage has passed for both fit_direct and needs_decompose"
brief: "Build plan and status ledger for the phase-decompose workflows, scout agent, evidence sizing tool, and recomposition path."
---

# Phase Decomposer — Implementation Plan

Date: 2026-05-10
Status: implemented after live no-edit validation on 2026-05-16. Live smoke
coverage passed for both `fit_direct` and `needs_decompose`, including
measured-byte DAG lint, guarded no-edit foreach, and recompose-time assertions.
Final hardened live proof: `arc-5a5fd112da724ce7a06ab7d1fe007bd8` reached
`Done` with `recompose_verdict=satisfied`. Edit/merge mediation is explicitly
out of v1 rather than a shipped Phase 7 claim.
Companion to: `design/orchestration/phase-decomposer/phase-decomposer.md` (pure design - this is the build plan).
Depends on: `design/orchestration/supervision/supervision-phased-implementation.md` (supervised atom
orchestration primitives must exist before Phase 6 foreach implementer
dispatch).

## Implementation Status

| Phase | Status | Notes |
|---|---|---|
| 1. `bbox_ref_size` MCP tool | **Done** | Tool handler in `src/tools/graph.rs`; implementation in `src/mcp_tools/ref_size.rs`; project-file full-content lookup in `src/index/mod.rs`; docs in `src/tool_docs.rs`. |
| 2. Scout agent manifest | **Done** | `system-defaults/agents/corpus-pathfinder.json`; reconciles the reverted Claude subagent prompt with Badgey's scout contract and atom-style grounding discipline. |
| 3. Inlet agent | **Done** | `phase-decompose-discovery` v9 plus `phase-decomposer-inlet`; scouts feed `bbox_ref_size`, inlet emits `evidence_bundle` + `triage_verdict`, and does not construct a DAG. |
| 4. Single-implementer path | **Done** | `phase-decompose-supervised-impl` plus `phase-decompose-main` direct branch. Live direct smoke passed after `InitEpoch` hardening (`arc-6381ec7ba9c34201b427897cd40884a5`). |
| 5. Ensemble decomposition | **Done** | `phase-decompose-ensemble-decompose` v20, `phase-decomposer-panel`, whiteboard packets, facilitator strict-DAG synthesis, mechanical `lint-dag.py` byte/coverage validation, `normalize-dag-measurements.py` derived-byte normalization, degraded-ref carry-through, and explicit terminal-verdict taxonomy handling. |
| 6. Foreach implementer dispatch | **Done** | `phase-decompose-main` foreaches over `vars.dag.sub_units` into `phase-decompose-supervised-impl` and collects sub-results. |
| 7. Recomposition council + remediation | **Done** | `phase-decompose-recompose` v6, `phase-recompose-council`, verdict packet, remediation packet back-edge, epoch-ceiling routing, stdin-backed mechanical recompose assertions, and arc-id status observability. Final live decomposed smoke passed (`arc-5a5fd112da724ce7a06ab7d1fe007bd8`). Edit/merge mediation is out of v1. |

The decomposer is mostly **configuration** on top of existing workflow
engine primitives. The engine already has `foreach`, `subworkflow`,
`Branch`, `Fork`, `Wait`, `gate`, and `durable` actors. Whiteboard
deliberation uses the `whiteboard_*` MCP tool surface
(`src/whiteboards.rs`), not an engine primitive — workflows call it
via `mcp_call` hook-ops. The new
artifacts are: one MCP tool, several agent manifests, brofiles, teamplates,
packet definitions, and workflow JSON artifacts.

```
Phase 1 ──┐
          ├──▶ Phase 3 ──┬──▶ Phase 4 ──┐
Phase 2 ──┘              │              ├──▶ Phase 6 ──▶ Phase 7
                         └──▶ Phase 5 ──┘

The reusable supervision primitives in
`design/orchestration/supervision/supervision-phased-implementation.md` exist and are composed by
Phase 6 (foreach implementers run inside supervised subworkflows).
```

---

## Phase 1: `bbox_ref_size` MCP tool

> **Status: shipped.** The live tool accepts up to 500 refs, canonicalizes
> successful refs, reports unresolved/omitted refs under `degraded`, measures
> full indexed chunk content for `project_file` / `project_file_v2`, and
> measures provider-properties JSON for other entity refs.

**Prerequisites:** none.

**What shipped:**

1.1 **MCP tool handler.** `bbox_ref_size(refs: [String]) -> {total_bytes:
   u64, per_ref: [{ref: String, bytes: u64}], degraded: {...}}`. Resolves
   each entity_ref or project_file_ref to the payload that downstream phase
   routing will actually receive. For `project_file` and `project_file_v2`
   refs, it resolves the indexed chunk and measures the full `content` bytes.
   For non-file entity refs, it resolves through registered entity providers
   and measures the serialized provider-properties JSON.

1.2 **Resolution.** Reuses `entity_ref::EntityRef::parse`
   (`entity_ref.rs`). For project-file refs, it uses the index lookup path
   added for this phase to fetch `EmbeddingSourceDoc.content`; this avoids
   measuring only `content_preview`. For non-file refs, it uses the entity
   provider registry directly rather than measuring an expanded neighborhood
   view.

1.3 **Batching.** Accepts up to 500 refs per call. Returns total +
   per-ref breakdown, with unresolved refs and over-cap omissions reported
   under `degraded`.

**Deliverable:** `bbox_ref_size(["project_file:<pid>:<h>:<c>:0",
"knowledge:<id>"])` returns `{total_bytes: 3035, per_ref: [{ref: ...,
bytes: 2140}, {ref: ..., bytes: 895}]}`.

**Estimated size:** ~100-150 lines of Rust (handler, resolution, batching).

---

## Phase 2: Scout agent manifest

> **Status: shipped.** `corpus-pathfinder` is installed as a generic agent
> manifest in `system-defaults/agents/corpus-pathfinder.json`. It pulls the
> useful intent from the reverted Claude Code subagent
> (`3dbc7e9:.claude/agents/corpus-pathfinder.md`) but does not resurrect that
> prompt-only enforcement model.

**Prerequisites:** none. Can proceed in parallel with Phase 1.

**What shipped:**

2.1 **Corpus-pathfinder as JSON agent.** The old
   `.claude/agents/corpus-pathfinder.md` was intentionally removed in
   `ad36da6` because Claude did not reliably honor prompt-enforced search/read
   caps. The shipped artifact preserves its durable intent instead:
   one focused graph-grounding scout that returns round-trippable entity refs,
   validated path ids, evidence-bundle handles, limits, and gaps.

   The manifest follows the `code-reviewer.json` / `diff-narrator.json`
   schema under `system-defaults/agents/`, with these reconciliations:

   - Default provider is Codex (`codex`, `gpt-5.5`, medium), because the
     removal commit records Codex as the provider that actually followed the
     grounding contract in the original trials.
   - The lens keeps the old pathfinder loop:
     `bbox_describe_schema` when needed, `bbox_hybrid_search`,
     `bbox_inspect_entity`, `bbox_find_paths` for multi-hop claims, and
     `bbox_bundle_evidence` before returning.
   - The behavior matches `system-defaults/badgey/agents/badgey-scout.json`
     at the conceptual level: one focused investigation, no sub-agent spawn,
     no synthesis beyond evidence. It deliberately differs by returning
     structured JSON directly instead of emitting `bbox_note(kind="done")`,
     because Phase 3 aggregates `vars.scout_results` mechanically.
   - It can read grounding atoms (`atom_list`, `atom_get`, `atom_describe`,
     `atom_search`) so scout output can point downstream consumers at existing
     atomized analysis tools rather than inventing manual tool sequences.

   Fields:
   - `description`: "Scout for codebase exploration..."
   - `when_to_use`: ["when dispatching discovery scouts over a phase doc",
     "when grounding a proposal in code before implementing"]
   - `anti_patterns`: ["do not use for one-line lookups",
     "do not use when the answer is already known"]
   - `brofile_inline`: provider/model/effort + reconciled pathfinder lens.
   - `filter_overlay.disallow`: ["Edit", "Write", "Bash",
     "NotebookEdit", "mcp__blackbox__bro_*",
     "mcp__blackbox__bbox_learn", "mcp__blackbox__bbox_remember",
     "mcp__blackbox__bbox_decide", "mcp__blackbox__bbox_forget"]
   - `inputs.schema`: `{question_shape, query, scope_hint?,
     known_evidence?}`
   - `outputs.schema`: strict-typed JSON — `{tldr, leads_symbols_files,
     leads_entity_refs, leads_paths, next_hops, bundle_handle, limits,
     gap_check}`
   - `composition.parallel_safe: true`
   - `composition.fan_out_aggregator: "ensemble-merge"`
   - `cost_class: "cheap"`

2.2 **Provider routing.** Primary provider is Codex (`gpt-5.5`, medium).
   Claude can be added later as an alternate only if the runner can enforce
   the same read-only graph surface mechanically; prompt-only discipline was
   already tested and removed. The agent manifest declares the brofile inline;
   `bro_agent_dispatch` resolves the provider.

**Deliverable:** `bro_agent_dispatch(agent="corpus-pathfinder",
args={query: "find auth middleware", question_shape: "WHERE"})` returns
structured leads JSON. Installable via `bbox_artifact_install
kind=agent`.

**Actual size:** 1 JSON artifact. No Rust code.

---

## Phase 3: Inlet agent (discovery subworkflow)

**Prerequisites:** Phase 1 (`bbox_ref_size` tool), Phase 2 (scout agent).

**What gets built:**

3.1 **Inlet brofile.** A brofile for the large-context orchestrator
   (Opus 4.7 [1M] or Codex). Lens: "You are a discovery orchestrator.
   Read the phase doc. Extract question-shapes. Dispatch scouts. Read
   their results. Aggregate into an evidence manifest. Call
   `bbox_ref_size`. Produce a triage verdict."

3.2 **Discovery subworkflow.** A workflow JSON artifact
   (`system-defaults/workflows/phase-decompose/discovery.json`), installed via
   `bro_workflow_install`. Three nodes:

   - **Parse** (hook-only): `shell` hook-op extracts question-shapes
     from phase doc frontmatter. Writes to `vars.question_shapes`.
   - **Scout foreach**: `foreach` over `vars.question_shapes`. Each
     iteration dispatches `corpus-pathfinder` agent via
     `bro_agent_dispatch`. Results collect into `vars.scout_results`.
   - **Inlet** (durable Executor): reads `vars.scout_results`.
     Aggregates, deduplicates, calls `bbox_ref_size`, produces
     `vars.evidence_bundle` and `vars.triage_verdict`. DAG construction
     belongs to the decomposer/ensemble path after discovery.

3.3 **Parent gate packet.** After the discovery subworkflow exports
   `triage_verdict` to the parent, the PARENT node carries a `gate`
   packet (`domain:phase-decompose/triage`). Reads
   `vars.triage_verdict` and emits `fit_direct` or
   `needs_decompose`. Subworkflow gate verdicts are not promoted
   (the engine exports vars only) — the parent must have its
   own gate. The parent's `Branch` routes on `last_verdict`.

3.4 **Parent workflow integration.** The parent workflow imports
   `phase_doc_path` into the discovery subworkflow. On completion,
   exports `evidence_bundle` and `triage_verdict` are promoted back to
   parent vars by the engine's subworkflow export path. The parent's next
   node carries a gate packet that reads `vars.triage_verdict` and
   emits the classification. `Branch` routes on `last_verdict`.
   **Subworkflow gate verdicts are not promoted** — the parent needs
   its own gate after export.

**Deliverable:** A phase doc entering the discovery subworkflow
produces a measured evidence bundle and a triage verdict. The parent
branches correctly on `fit_direct` vs `needs_decompose`.

**Estimated size:** 1 brofile (~30 lines), 1 workflow JSON artifact
(~100-150 lines), 1 gate packet (~30 lines). No new Rust code.

---

## Phase 4: Single-implementer path (fit_direct)

**Prerequisites:** Phase 3 (discovery subworkflow produces evidence
bundle + triage verdict). Supervision plan normalization, polling,
classifier/advisor workflow-backed atom patterns, and typed advisor action
execution must exist.

**What gets built:**

4.1 **Supervised implementer subworkflow.** A reusable subworkflow
   artifact (`system-defaults/workflows/phase-decompose/supervised-impl.json`).
   Imports: `vars.brofile`, `vars.prompt`, `vars.evidence_manifest`,
   `vars.acceptance_criteria`. Inside:
   - Implementer dispatch (fire-and-forget)
   - Optional classifier workflow-backed atom polling the implementer
   - Advisor workflow-backed atom at turn end or classifier alert
   - Branch on advisor action

4.2 **Direct-implementer workflow node.** After the discovery
   subworkflow returns `fit_direct`, a node runs
   `subworkflow_ref: "supervised-impl"` with the inlet's evidence
   bundle as the manifest.

4.3 **End-to-end test.** A small phase doc (e.g., "add dark mode
   toggle") → discovery → `fit_direct` → supervised implementer
   → advisor → acceptance gate → done.

**Deliverable:** A trivial phase doc flows through the full pipeline:
discovery → triage → implementer → advisor → done. No decomposition
machinery yet.

**Estimated size:** 1 workflow JSON artifact (~80-100 lines). No new
Rust code.

---

## Phase 5: Ensemble decomposition (needs_decompose)

**Prerequisites:** Phase 3 (discovery subworkflow). Can proceed in
parallel with Phase 4.

**What gets built:**

5.1 **Decomposer teamplate.** A teamplate
   (`system-defaults/phase-decompose/teamplates/decomposer-panel.json`).
   Members: 2-3 specialist brofiles (e.g., `decomposer-security`,
   `decomposer-architecture`, `decomposer-performance`). The actor
   kind for the node is `Ensemble`, which broadcasts
   to the team.

5.2 **Decomposer brofiles.** One brofile per specialist role. Lens:
   "You are a decomposition specialist for <domain>. Read the phase
   doc and evidence bundle. Post a proposed decomposition to the
   whiteboard. Use `bbox_ref_size` to measure each proposed sub-unit."

5.3 **Decomposer workflow nodes.** Following the `whiteboard-arc.json`
   pattern (`examples/whiteboard/workflows/whiteboard-arc.json`):
   - **OpenBoard**: hook-only, creates whiteboard, registers members.
   - **BlindPost**: Ensemble broadcast. Each member posts a proposed
     decomposition independently.
   - **Debate**: Ensemble broadcast. Members read each other's posts,
     annotate, vote.
   - **TransitionToResolve**: hook-only, transitions board to resolve.
   - **Synthesize**: Executor (facilitator brofile). Reads
     `whiteboard_summarize`. Emits the DAG artifact
     (`vars.dag`). Uses `bbox_ref_size` cluster-by-cluster to
     validate sub-unit sizes. In v1, `target_context_window` means the
     per-sub-unit measured evidence payload budget returned by
     `bbox_ref_size`; it excludes fixed workflow prompt, brofile, ambient
     scope, and MCP-injection overhead.

5.4 **DAG validation.** A gate packet on the Synthesize node verifies
   the DAG shape (required fields present, sub_units non-empty,
   merge_order matches sub_unit_ids). **Coverage and measured-byte lint
   cannot be fully expressed as packet rules today** — `ForAll`
   quantifies over one array path but cannot correlate an
   outer `criterion_id` into an inner `Exists` over sibling `sub_units[*]`
   acceptance subsets, and packet rules cannot call `bbox_ref_size`.
   `SynthesizeDag/on_exit` therefore extracts DAG refs, calls
   `bbox_ref_size`, and runs `lint-dag.py`. The lint fails on missing
   acceptance coverage, degraded ref measurement, declared bytes that differ
   from measured ref bytes, and measured bytes over `target_context_window`.
   The packet gate handles structural validation; coverage and byte accuracy
   are mechanical hook validation in v1.

**Deliverable:** A `needs_decompose` verdict routes to the decomposer
panel. The panel produces a validated DAG with per-sub-unit refs,
acceptance subsets, and measured byte sizes.

**Estimated size:** 1 teamplate (~30 lines), 2-3 brofiles (~90 lines),
   workflow nodes in the parent workflow (~150 lines), 1 gate packet
   (~40 lines). No new Rust code.

---

## Phase 6: Foreach implementer dispatch

**Prerequisites:** Phase 4 (supervised subworkflow template), Phase 5
(DAG artifact). Supervision S1, S2, S3, S5, S6, S7, and the needed S8 action
subset must be complete.

**What gets built:**

6.1 **Foreach over DAG.** After the decomposer produces `vars.dag`,
   a `foreach` node iterates over `vars.dag.sub_units`. Each
   iteration runs `subworkflow_ref:
   "phase-decompose-supervised-impl"` with the fixed bounded
   implementer/advisor pair:
   - `sub_unit`: the current DAG sub-unit, including its acceptance subset
     and refs.
   - `evidence_bundle`: the parent evidence bundle.
   - `acceptance_criteria`: the parent acceptance criteria; the sub-unit's
     scope is carried by `sub_unit.acceptance_subset`.

   DAG entries must not carry `assigned_brofile`; bounded execution is
   controlled by the supervised-impl workflow and brofiles.

6.2 **Collect outcomes.** `foreach.collect.into_var: sub_results`.
   Each outcome is a `FanoutChildOutcome` with `{status, exports,
   outputs}`. The sub-unit's advisor verdict and acceptance status
   are in `exports` (declared in `foreach.exports`).

6.3 **Parallelism.** Disjoint sub-units (no symbol overlap in
   predicted writes) run concurrently via `foreach.parallelism`.
   Overlapping sub-units serialize. `foreach` does not natively
   support per-item `depends_on` ordering — it dispatches items
   from an array. For DAG dependencies, either:
   - Topological-sort the sub_units array before foreach (items
     earlier in the array dispatch first; with `parallelism: 1`
     this enforces ordering).
   - Split into sequential foreach batches (one per topological
     level of the DAG).

6.4 **Integration test.** A multi-sub-unit phase doc flows through:
   discovery → decompose → foreach implementers → collect outcomes.

**Deliverable:** Multiple sub-units dispatch in parallel via foreach.
Each runs inside a supervised subworkflow. Outcomes collect correctly
with verdicts in exports.

**Estimated size:** Parent workflow nodes (~50-80 lines). No new Rust
code (foreach exists in the engine).

---

## Phase 7: Recomposition council + remediation

**Prerequisites:** Phase 6 (foreach implementer outcomes). Phases 4-5
for the supervision/adversarial patterns.

**What gets built:**

7.1 **Recomposition council teamplate.** A teamplate separate from
   the decomposer panel. Members: integration-specialist brofiles.
   `durable: true` — session persists across epochs (within the same
   workflow runner, via `Goto` back-edge). **Does not persist across
   fresh arcs or subworkflow boundaries**.

7.2 **Council evaluation node.** After foreach collects, the council
   (durable Ensemble) reads `vars.sub_results`. It evaluates:
   - Which sub-units passed their advisors + acceptance gates?
   - Which failed?
   - Which passed individually but still leave integration or acceptance
     seams for the next epoch?

   The council produces a verdict:
   - **Satisfied**: all passed, integration verified → `EXIT_MET`.
   - **Work remains**: some failed or need remediation → produce a
     **remediation packet** (new phase doc for remaining work).
     Route via `Goto` back-edge to the inlet.
   - **Untenable**: repeated failures, budget exhausted → halt with
     escalation note.

7.3 **Remediation packet.** A structured JSON payload describing
   remaining work: which acceptance criteria are unmet, which
   sub-units need re-dispatch, what conflicts surfaced. The packet
   re-enters the inlet → decomposer → dispatch → advisor gate →
   council loop.

7.4 **Edit/merge mediation is out of v1.** The shipped workflow does not
   merge sub-unit branches or dispatch conflict/regression repair agents.
   It has no branch-handle contract from foreach children, so claiming M1-M4
   here would be hollow. Integration failures are represented as
   `work_remains` plus a remediation packet that re-enters the inlet.

7.5 **Epoch ceiling.** `max_epochs` is not a `NodeSpec` field.
   The shipped workflow initializes
   `vars.epoch`, runs `system-defaults/phase-decompose/scripts/epoch-check.py`
   to compute `vars.epoch_status`, and routes through the
   `domain:phase-decompose/epoch-ceiling` packet. The packet reads
   `epoch_status=continue|halt` instead of hardcoding a numeric ceiling,
   so `max_epochs` remains runtime-configurable. The remediation back-edge
   increments `vars.epoch` before re-entering discovery.

7.6 **End-to-end test.** A phase doc requiring decomposition → inlet
   → decompose → DAG → foreach implementers → council evaluates →
   remediation packet → re-enters inlet → re-dispatches → council
   evaluates again → satisfied → done.

**Deliverable:** A multi-sub-unit phase with remaining work is detected by
the council, converted into a remediation packet, and resolved iteratively.
A phase that truly can't converge halts after the epoch ceiling.

**Estimated size:** 1 teamplate (~30 lines), council brofiles (~60
lines), parent workflow nodes for council/remediation (~150 lines), 1 gate
packet for epoch ceiling (~20 lines). No new Rust code.

---

## Build sequence summary

| Phase | Can start after | New Rust | New artifacts | Test |
|---|---|---|---|---|
| 1. bbox_ref_size | — | ~100-150 lines | — | Resolve ref → byte payload |
| 2. Scout manifest | — | — | 1 agent JSON | bro_agent_dispatch returns structured leads |
| 3. Inlet agent | 1, 2 | — | 1 brofile, 1 workflow, 1 packet | Phase doc → evidence bundle + triage verdict |
| 4. Single-implementer | 3, supervision S1, S2, S3, S5, S7, S8 subset | — | 1 workflow | fit_direct -> implementer -> advisor -> done |
| 5. Ensemble decompose | 3 | — | 1 teamplate, 2-3 brofiles, 1 packet | needs_decompose → whiteboard → validated DAG |
| 6. Foreach implementers | 4, 5, supervision S6-S8 | — | parent workflow nodes | DAG sub-units -> foreach -> collect outcomes |
| 7. Recompose council | 6, supervision S9 | — | 1 teamplate, 2 brofiles, 1 packet | Work remains → remediation packet → converge or halt |

Additional Rust code after Phase 1: `src/dispatch_mcp.rs` now injects the
`agent-internal` MCP surface for dispatched bros so whiteboard tools are
visible, and `src/workflow/ops.rs` now makes `parse_json` robust to
live-agent preambles before inline JSON. The remaining implementation is
configuration artifacts on top of the existing workflow engine.

## Live validation

Current smoke runs:

- Direct path: `arc-2720d7cf32f84bddb3b2bf9d716fd20e`, completed before
  `InitEpoch` hardening.
- Direct path after `InitEpoch` hardening:
  `arc-6381ec7ba9c34201b427897cd40884a5`, completed,
  `path=fit_direct`, `triage_verdict=fit_direct`, `acceptance_status=passed`,
  `evidence_total_bytes=14931`, `target_context_window=1000000`, unresolved
  refs `[]`.
- Earlier decomposed path: `arc-4c09aa3a93c549aa9a722fd4cb307257`, completed
  before the fail-closed DAG lint hardening.
- Decomposed path after fail-closed DAG lint and epoch-ceiling hardening:
  `arc-67949c254c264a0687941f182509ea50`, completed, `path=decomposed`,
  `triage_verdict=needs_decompose`, `recompose_verdict=satisfied`, sub-unit
  bytes `[8000, 8500, 9500]` against a `10000` target, all sub-results
  completed.
- Earlier final decomposed live no-edit smoke:
  `arc-4ed97fce194e45d8a4d875fad71b5471` / `thread-c527a9aa`, completed,
  `path=decomposed`, `triage_verdict=needs_decompose`,
  `recompose_verdict=satisfied`. This run used `phase-decompose-main` v3,
  `phase-decompose-supervised-impl` v4, and `phase-decompose-recompose` v4.
  The ensemble DAG lint measured every sub-unit ref via `bbox_ref_size`; all
  five supervised subflows completed through the no-edit diff guard; recompose
  asserted the `sub_results` field-shape bridge, sub-unit count/key coverage,
  terminal verdict set, empty `files_touched`, and matching parent
  `live_arc_id`.
- Final installed v10/v5 decomposed live no-edit smoke:
  `arc-e3c6ad687d8d46419baab367a7330810` / `thread-3ea7996c`, completed,
  `path=decomposed`, `triage_verdict=needs_decompose`,
  `recompose_verdict=satisfied`, `sub_results_count=3`. This run used
  `phase-decompose-main` v3, `phase-decompose-discovery` v8,
  `phase-decompose-ensemble-decompose` v10,
  `phase-decompose-supervised-impl` v4, and `phase-decompose-recompose` v5.
  The inlet reported `degraded.unresolved_refs=[]`; the v10 debate resolved
  whiteboard challenges before `ValidateDebate`; the facilitator emitted a
  three-sub-unit DAG with measured bytes `[8877, 7707, 9666]` against a
  `10000` target, `degraded_refs=[]`, and
  `terminal_verdicts=["satisfied","work_remains","untenable"]` as the allowed
  recompose taxonomy rather than a predicted outcome list. All three
  supervised subflows completed through the no-edit guard with
  `files_touched=[]`; recompose v5 returned `satisfied` after stdin-backed
  assertions over the DAG, sub-results, acceptance criteria, terminal verdict
  taxonomy, and parent `live_arc_id`.
- Final hardened v17/v6 decomposed live no-edit smoke:
  `arc-f1073863fc974e95804d18e8c3e018f9` / `thread-24b87542`, completed,
  `path=decomposed`, `triage_verdict=needs_decompose`,
  `recompose_verdict=satisfied`, `last_verdict=satisfied`. This run used
  `phase-decompose-main` v4, `phase-decompose-discovery` v9,
  `phase-decompose-ensemble-decompose` v17,
  `phase-decompose-supervised-impl` v7, and `phase-decompose-recompose` v6.
  The inlet reported `evidence_bundle.total_bytes=72544`,
  `target_context_window=10000`, and no unresolved degraded refs in the final
  evidence bundle. The ensemble debate converged through the whiteboard, then
  `lint-dag.py` accepted a five-sub-unit DAG with measured bytes
  `[9416, 9559, 9818, 9164, 8122]` against the `10000` target and
  `degraded_refs=[]`. All five supervised no-edit subflows completed with
  advisor verdict `accept`, `acceptance_status=passed`, matching
  `live_arc_id`, and `files_touched=[]`. Recompose v6 returned
  `satisfied`; parent `RecomposeGate` accepted the terminal verdict and the
  parent workflow reached `Done` and `(end)`.
- Final generalized v20/v6 decomposed live no-edit smoke:
  `arc-5a5fd112da724ce7a06ab7d1fe007bd8` / `thread-77c29fca`,
  completed, `path=decomposed`, `triage_verdict=needs_decompose`,
  `recompose_verdict=satisfied`, `last_verdict=satisfied`. This run used
  `phase-decompose-main` v4, `phase-decompose-discovery` v9,
  `phase-decompose-ensemble-decompose` v20,
  `phase-decompose-supervised-impl` v7, and `phase-decompose-recompose` v6.
  The inlet reported `evidence_bundle.total_bytes=76940`,
  `target_context_window=10000`, and `degraded.unresolved_refs=[]`. The
  ensemble workflow no longer carries fixture-specific ref requirements:
  `lint-dag.py` requires all resolved evidence refs to appear in the final DAG,
  rejects unexpected degraded refs, and the workflow runs
  `normalize-dag-measurements.py` before lint so derived `bytes` fields and
  degraded refs are normalized from `bbox_ref_size` truth rather than copied
  scout/content-preview numbers. `lint-dag.py` accepted eight sub-units with
  measured bytes `[9388, 9677, 9815, 8751, 9925, 9876, 9690, 9818]` against
  the `10000` target and `degraded_refs=[]`. All eight supervised no-edit
  subflows completed; foreach collected all eight with `failed=false`.
  Recompose v6 returned `satisfied`; parent `RecomposeGate` accepted the
  terminal verdict and the parent workflow reached `Done` and `(end)`.
- Negative live proof before the v17 prompt/lint tightening:
  an earlier run failed mechanically when the facilitator emitted oversized
  units (`17526` and `14719` bytes against a `10000` target), and another was
  cancelled after `ensemble-decompose.json` was accidentally pretty-printed to
  `13934` bytes. These failures are kept as evidence that the byte gate fails
  closed rather than accepting optimistic grouping.
- Negative live proof before the v20 normalization hardening: Claude and
  DeepSeek review caught fixture-specific prompt/lint behavior. After removing
  the fixture-specific required-ref list, two live attempts failed mechanically
  when the facilitator copied stale scout byte counts and invented degraded refs
  despite `bbox_ref_size` resolving those refs. v20 keeps those failures useful
  by deriving byte fields from measured refs and rejecting unexpected degraded
  refs before any foreach dispatch.
- Post-run substrate fix: `bro orchestrate status <arc-id>` now resolves the
  stable workflow `arc-*` id to the corresponding `thread-*` note trail while
  the arc snapshot is present. Smoke:
  `arc-81a0df1191fd4fe48087d7d97c2e7dee` resolved to `thread-57fe9cec` and
  returned the same notes as querying the thread directly. This closes the
  live-arc observability bug surfaced by the recompose council.
- Post-run runtime cleanup fix: workflow `mcp_call` now uses bounded
  `close_with_timeout` cleanup after receiving a tool result so loopback MCP
  shutdown cannot spend the entire call timeout after a successful response.
- Post-run `/orchestrate/by-id` fix: the HTTP route now starts the installed
  workflow as a pollable task and returns `taskId` / `arcId` immediately by
  default, matching `bro_orchestrate_run`; blocking behavior is explicit via
  `await_completion=true`. Smoke: a `max_steps=0` POST for
  `phase-decompose-main` returned immediately with
  `arc-c84548887db14067b625211ed4132061` instead of hanging the HTTP request.
- Edit/merge mediation is not part of v1 and no longer appears as a shipped
  artifact claim in this plan.

Required external review drove the final hardening pass:

- Claude Opus 4.7 xhigh follow-up task
  `f57ac6a0-3d77-46a7-ad44-f2d7c6906dc9`: found blockers for untracked
  scripts, a stale supervision cross-reference, and fixture-specific
  facilitator/linter behavior. The final tree stages the scripts, fixes the
  cross-reference, replaces fixture-specific required refs with generic
  evidence-ref coverage, and adds v20 normalization/lint hardening.
- DeepSeek V4 Pro follow-up task
  `299ef37a-d02b-4cce-ad7d-abe39453c8fc`: found blockers for `__pycache__`
  worktree noise and the same stale supervision cross-reference. The final tree
  adds `__pycache__/` and `*.pyc` ignores and fixes the cross-reference.

## What already exists (no new code needed)

| Primitive | Used by Phase | Location |
|---|---|---|
| foreach + collect | 3, 6 | `src/workflow/schema.rs`, `src/workflow/engine.rs` |
| subworkflow + imports/exports | 3, 4, 6 | `src/workflow/engine.rs` |
| Branch (gate verdict routing) | 3, 4, 6, 7 | `src/workflow/schema.rs`, `src/workflow/engine.rs` |
| Fork (fire-and-forget) | 6 | `src/workflow/schema.rs`, `src/workflow/engine.rs` |
| Wait (signal suspension) | 6 | `src/workflow/wait.rs` |
| Goto (back-edge for loops) | 7 | `src/workflow/schema.rs` |
| Node gate + gate_mode | 3, 5, 7 | `src/workflow/schema.rs` |
| Whiteboard (MCP tool surface) | 5, 7 | `src/whiteboards.rs` (store), `whiteboard_*` MCP tools |
| Durable actor sessions | 3, 5, 7 | `src/workflow/schema.rs` |
| Agent manifests | 2, 5, 7 | `system-defaults/agents/code-reviewer.json` |
| bro_agent_dispatch | 2, 3 | `src/tools/agents.rs` |
| bro_workflow_install | 3, 4 | `src/tools/orchestrate.rs` |
| bbox_compile / bbox_audit | 3, 5, 7 | `src/tools/packets.rs` |
| entity_ref::EntityRef | 1 | `src/entity_ref.rs` |
| entity_loader::load | 1 | `src/entity_loader.rs` |

## Dependency on supervision-phased-implementation.md

Phases 4, 6, and 7 require the supervision infrastructure from
`design/orchestration/supervision/supervision-phased-implementation.md`:

- Phase 4 needs advisor-only supervision: S1 normalize-plan, S2 attachment
  model, S3 polling primitive, S5 structured exit, S7 advisor atom, and the S8 action
  subset for `accept`, `steer_primary`, and `bail`.
- Phase 6 additionally needs S6 classifier support and S8 action execution
  available inside foreach subworkflows.
- Phase 7 additionally needs S9 tier-recovery/runtime allocation patterns for
  remediation re-entry. Edit/merge mediation is a separate future design.

The decomposer implementation plan can begin from the assumption that the
supervision primitives through S9 have landed. Phase 4 onward should still
exercise the live advisor/classifier paths in its own workflow fixtures, because
the decomposer composes those primitives rather than owning them.

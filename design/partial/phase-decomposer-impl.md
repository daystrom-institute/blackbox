# Phase Decomposer — Implementation Plan

Date: 2026-05-10
Status: partially implemented — Phases 1-2 shipped; Phases 3-7 remain open.
Companion to: `design/partial/phase-decomposer.md` (pure design - this is the build plan).
Depends on: `design/archive/supervision-phased-implementation.md` (supervised atom
orchestration primitives must exist before Phase 6 foreach implementer
dispatch).

## Implementation Status

| Phase | Status | Notes |
|---|---|---|
| 1. `bbox_ref_size` MCP tool | **Done** | Tool handler in `src/tools/graph.rs`; implementation in `src/mcp_tools/ref_size.rs`; project-file full-content lookup in `src/index/mod.rs`; docs in `src/tool_docs.rs`. |
| 2. Scout agent manifest | **Done** | `system-defaults/agents/corpus-pathfinder.json`; reconciles the reverted Claude subagent prompt with Badgey's scout contract and atom-style grounding discipline. |
| 3. Inlet agent | **Not built** | Depends on Phases 1-2. |
| 4. Single-implementer path | **Not built** | Depends on Phase 3. |
| 5. Ensemble decomposition | **Not built** | Depends on Phase 3. |
| 6. Foreach implementer dispatch | **Not built** | Depends on Phases 4-5 and supervision primitives. |
| 7. Recomposition council + mediation | **Not built** | Depends on Phase 6. |

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
`design/archive/supervision-phased-implementation.md` must exist before
decomposer Phase 6 (foreach implementers run inside supervised subworkflows).
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
   (`examples/phase-decompose/workflows/discovery.json`), installed via
   `bro_workflow_install`. Three nodes:

   - **Parse** (hook-only): `shell` hook-op extracts question-shapes
     from phase doc frontmatter. Writes to `vars.question_shapes`.
   - **Scout foreach**: `foreach` over `vars.question_shapes`. Each
     iteration dispatches `corpus-pathfinder` agent via
     `bro_agent_dispatch`. Results collect into `vars.scout_results`.
   - **Inlet** (durable Executor): reads `vars.scout_results`.
     Aggregates, deduplicates, calls `bbox_ref_size`, produces
     `vars.evidence_bundle`, `vars.triage_verdict`,
     `vars.dag_sketch`.

3.3 **Parent gate packet.** After the discovery subworkflow exports
   `triage_verdict` to the parent, the PARENT node carries a `gate`
   packet (`domain:phase-decompose/triage`). Reads
   `vars.triage_verdict` and emits `fit_direct` or
   `needs_decompose`. Subworkflow gate verdicts are not promoted
   (`engine.rs:2543` exports vars only) — the parent must have its
   own gate. The parent's `Branch` routes on `last_verdict`.

3.4 **Parent workflow integration.** The parent workflow imports
   `phase_doc_path` into the discovery subworkflow. On completion,
   exports `evidence_bundle`, `triage_verdict`, `dag_sketch` are
   promoted back to parent vars (`engine.rs:2543`). The parent's next
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
   artifact (`examples/phase-decompose/workflows/supervised-impl.json`).
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
   (`examples/phase-decompose/teamplates/decomposer-panel.json`).
   Members: 2-3 specialist brofiles (e.g., `decomposer-security`,
   `decomposer-architecture`, `decomposer-performance`). The actor
   kind for the node is `Ensemble` (`schema.rs:99`), which broadcasts
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
     validate sub-unit sizes.

5.4 **DAG validation.** A gate packet on the Synthesize node verifies
   the DAG shape (required fields present, sub_units non-empty,
   merge_order matches sub_unit_ids). **Coverage lint cannot be fully
   expressed as a packet rule today** — `ForAll` (`ast.rs:211`)
   quantifies over one array path but cannot correlate an outer
   `criterion_id` into an inner `Exists` over sibling `sub_units[*]`
   acceptance subsets. The coverage lint is a `shell` hook-op
   or a future typed `lint_acceptance` op. The gate packet handles
   structural validation; coverage is mechanical but not purely
   packet-driven in v1.

**Deliverable:** A `needs_decompose` verdict routes to the decomposer
panel. The panel produces a validated DAG with per-sub-unit refs,
acceptance subsets, and measured byte sizes.

**Estimated size:** 1 teamplate (~30 lines), 2-3 brofiles (~90 lines),
   workflow nodes in the parent workflow (~150 lines), 1 gate packet
   (~40 lines). No new Rust code.

---

## Phase 6: Foreach implementer dispatch

**Prerequisites:** Phase 4 (supervised subworkflow template), Phase 5
(DAG artifact). Supervision phases 1-6 must be complete.

**What gets built:**

6.1 **Foreach over DAG.** After the decomposer produces `vars.dag`,
   a `foreach` node iterates over `vars.dag.sub_units`. Each
   iteration runs `subworkflow_ref: "supervised-impl"` with:
   - `brofile`: the sub-unit's assigned implementer brofile
   - `prompt`: the sub-unit's acceptance criteria + evidence subset
   - `evidence_manifest`: the refs from the DAG for this sub-unit
   - `acceptance_criteria`: the sub-unit's `acceptance_subset`

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

## Phase 7: Recomposition council + mediation

**Prerequisites:** Phase 6 (foreach implementer outcomes). Phases 4-5
for the supervision/adversarial patterns.

**What gets built:**

7.1 **Recomposition council teamplate.** A teamplate separate from
   the decomposer panel. Members: integration-specialist brofiles.
   `durable: true` — session persists across epochs (within the same
   workflow runner, via `Goto` back-edge). **Does not persist across
   fresh arcs or subworkflow boundaries** (`engine.rs:600, 720`).

7.2 **Council evaluation node.** After foreach collects, the council
   (durable Ensemble) reads `vars.sub_results`. It evaluates:
   - Which sub-units passed their advisors + acceptance gates?
   - Which failed?
   - Which passed individually but conflict on integration?

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

7.4 **Mediation (M1-M4).** When sub-units passed individually but
   merge conflicts exist:
   - **M1**: `shell` hook-op runs `git merge` per branch.
   - **M2**: Conflict-resolver Executor. Produces concrete file edit.
     Surfaces unresolvable conflicts with explicit notes. Does NOT
     drop acceptance criteria.
   - **M3**: Mediation whiteboard panel. One advocate per conflicting
     sub-unit. Debates, votes. Facilitator resolves or declares
     deadlock.
   - **M4**: Regression-fixer. Runs test suite. Patches failures.
     Retries up to ceiling.

   The council reads M1-M4 outcomes. Resolved → satisfied. Deadlocked
   → remediation packet.

7.5 **Epoch ceiling.** `max_epochs` is not a `NodeSpec` field
   (`schema.rs:107-220`). Use an epoch counter in `vars.epoch` (set
   and incremented by the council node's `on_exit` hook) plus a gate
   packet that reads `Ge{vars.epoch, value: N}` → `halt`. Or reuse
   `retry.max_generations` (`schema.rs:348-353`) on the council node
   as a ceiling on council evaluations.

7.6 **End-to-end test.** A phase doc requiring decomposition → inlet
   → decompose → DAG → foreach implementers → council evaluates →
   remediation packet → re-enters inlet → re-dispatches → council
   evaluates again → satisfied → done.

**Deliverable:** A multi-sub-unit phase with an integration conflict
is detected by the council, mediated, and resolved iteratively. A
phase that truly can't converge halts after the epoch ceiling.

**Estimated size:** 1 teamplate (~30 lines), council brofiles (~60
lines), parent workflow nodes for council + mediation (~150-200
lines), 1 gate packet for epoch ceiling (~20 lines). No new Rust code.

---

## Build sequence summary

| Phase | Can start after | New Rust | New artifacts | Test |
|---|---|---|---|---|
| 1. bbox_ref_size | — | ~100-150 lines | — | Resolve ref → byte payload |
| 2. Scout manifest | — | — | 1 agent JSON | bro_agent_dispatch returns structured leads |
| 3. Inlet agent | 1, 2 | — | 1 brofile, 1 workflow, 1 packet | Phase doc → evidence bundle + triage verdict |
| 4. Single-implementer | 3, supervision P1, P2, P3a, P5, P6 subset | — | 1 workflow | fit_direct -> implementer -> advisor -> done |
| 5. Ensemble decompose | 3 | — | 1 teamplate, 2-3 brofiles, 1 packet | needs_decompose → whiteboard → validated DAG |
| 6. Foreach implementers | 4, 5, supervision P4-P6 | — | parent workflow nodes | DAG sub-units -> foreach -> collect outcomes |
| 7. Recompose council | 6, supervision P7 | — | 1 teamplate, 2 brofiles, 1 packet | Conflict → mediate → converge or halt |

Total new Rust code: ~100-150 lines (Phase 1 only). Everything else is
configuration artifacts on top of the existing workflow engine.

## What already exists (no new code needed)

| Primitive | Used by Phase | Location |
|---|---|---|
| foreach + collect | 3, 6 | `schema.rs:193-276`, `engine.rs:1662-1874` |
| subworkflow + imports/exports | 3, 4, 6 | `engine.rs:2401-2580` |
| Branch (gate verdict routing) | 3, 4, 6, 7 | `schema.rs:389-395` |
| Fork (fire-and-forget) | 6 | `schema.rs:396-403` |
| Wait (signal suspension) | 6 | `src/workflow/wait.rs` |
| Goto (back-edge for loops) | 7 | `schema.rs:384` |
| Node gate + gate_mode | 3, 5, 7 | `schema.rs:120-127` |
| Whiteboard (MCP tool surface) | 5, 7 | `src/whiteboards.rs` (store), `whiteboard_*` MCP tools |
| Durable actor sessions | 3, 5, 7 | `schema.rs:64` |
| Agent manifests | 2, 5, 7 | `system-defaults/agents/code-reviewer.json` |
| bro_agent_dispatch | 2, 3 | `src/tools/agents.rs` |
| bro_workflow_install | 3, 4 | `src/tools/orchestrate.rs` |
| bbox_compile / bbox_audit | 3, 5, 7 | `src/tools/packets.rs` |
| entity_ref::EntityRef | 1 | `src/entity_ref.rs` |
| entity_loader::load | 1 | `src/entity_loader.rs` |

## Dependency on supervision-phased-implementation.md

Phases 4, 6, and 7 require the supervision infrastructure from
`design/archive/supervision-phased-implementation.md`:

- Phase 4 needs advisor-only supervision: P1 plan normalization, P2
  attachment/polling, P3a structured exits, P5 advisor atom, and the P6 action
  subset for `accept`, `steer_primary`, and `bail`.
- Phase 6 additionally needs P4 classifier support and P6 action execution
  available inside foreach subworkflows.
- Phase 7 additionally needs P7 runtime allocation/recovery and mediation
  patterns (M1-M4, which use existing shell/Executor/whiteboard primitives).

The decomposer implementation plan can begin from the assumption that the
supervision primitives through P7 have landed. Phase 4 onward should still
exercise the live advisor/classifier paths in its own workflow fixtures, because
the decomposer composes those primitives rather than owning them.

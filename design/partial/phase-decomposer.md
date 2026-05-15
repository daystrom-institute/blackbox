# Phase Decomposer — context-budget-aware execution of large phased plans

Date: 2026-05-10
Status: partially implemented — Phase 1 `bbox_ref_size` has shipped; workflow
and agent-artifact phases remain open.
Predecessor archived at `design/archive/phase-decomposer.md`.

## 1. Problem

Some bro providers compact catastrophically. The signature failure is a
vibe-class bro burning its context window on exploration and re-reading a
phase doc, compacting, then dropping intent — 80% through with partial,
unverified work committed.

Naive responses: route around bad providers (loses their strengths), write
smaller phases (pushes burden onto human authors, breaks for third-party
plans), or intercept provider built-ins (provider built-in disablement is
partial/provider-specific; `src/orchestration/providers.rs:790-850`. Universal
interception is not wired today).

The general move: fit the work to the budget by scouting the actual load,
measuring it mechanically, and routing accordingly.

## 2. Architecture: two stages

```
  ┌─────────────────────────────────┐
  │ Phase doc arrives               │
  │ (webhook / manual signal / cron) │
  └───────────────┬─────────────────┘
                  │
  ┌───────────────▼─────────────────┐
  │ INLET AGENT                     │
  │ (durable Executor, large-ctx)   │
  │                                 │
  │ 1. Read phase doc               │
  │ 2. Extract question-shapes      │
  │ 3. Dispatch scouts (foreach)     │
  │ 4. Aggregate scout results      │
  │ 5. Build evidence manifest      │
  │ 6. Measure manifest via         │
  │    bbox_ref_size MCP tool       │
  │ 7. Emit triage verdict +        │
  │    evidence bundle + (optional) │
  │    DAG sketch                   │
  └───────────────┬─────────────────┘
                  │
        ┌─────────┴─────────┐
        ▼                   ▼
   fit_direct         needs_decompose
        │                   │
        ▼                   ▼
  ┌──────────┐      ┌──────────────┐
  │ Impl-    │      │ ENSEMBLE     │
  │ ementer  │      │ (whiteboard) │
  │ (seeded  │      │              │
  │ with     │      │ Panel delib- │
  │ exact    │      │ erates on    │
  │ manifest)│      │ sub-unit     │
  └──────────┘      │ boundaries.  │
                    │ Produces DAG │
                    │ using same   │
                    │ ref→size     │
                    │ tool deep.   │
                    └──────┬───────┘
                           │
                     ┌──────▼───────┐
                     │ foreach      │
                     │ implementers │  ◄──────────────────┐
                     │ (each in     │                     │
                     │ supervised   │                     │
                     │ subworkflow) │                     │
                     └──────┬───────┘                     │
                            │                             │
                     ┌──────▼───────┐                     │
                     │ RECOMPOSE    │                     │
                     │ COUNCIL      │                     │
                     │ (durable,    │                     │
                     │  maintains   │                     │
                     │  context     │                     │
                     │  across      │                     │
                     │  epochs)     │                     │
                     │              │                     │
                     │ evaluates    │                     │
                     │ collected    │                     │
                     │ results →    │                     │
                     │ if unsatisf. │                     │
                     │ → remediation│                     │
                     │   packet     │────────▶ INLET ────┘
                     │              │   (re-enters
                     │ if satis-    │    pipeline for
                     │ fied → done  │    remaining work)
                     │ if untenable │
                     │ → halt       │
                     └──────────────┘
```

Each implementer runs inside a **supervised subworkflow**
(`design/archive/supervision.md`): implemented mechanical telemetry observes
the implementer; an optional workflow-backed classifier atom can poll that
telemetry and task state; an optional advisor atom evaluates turn-end or alert
checkpoints and routes to acceptance, steering, recovery, replacement, human
escalation, or bail.

The recomposition council is a **durable ensemble** — it persists across
epochs, maintaining context about what succeeded, what failed, and what
remediation is still needed. After each batch of implementer results:

- If the phase doc is fully satisfied and integration is sound → done.
- If work remains or failures need remediation → the council produces a
  **remediation packet** (a new phase doc for the remaining work) and
  pushes it back through the inlet → decomposer → dispatch → advisor
  gate loop. The same council re-evaluates the next batch.
- If the council decides the plan is untenable (repeated failures, scope
  impossible, budget exhausted) → halt with an escalation note.

## 3. Stage 1: Inlet agent

### 3.1 What it does

A single durable Executor actor (`src/workflow/schema.rs:79-105` — the
actor kind used for single-bro dispatch; `Ensemble` is the other, for team
broadcasts. Roles are workflow-author concerns carried by brofile + prompt).
Large-context model (Opus 4.7 [1M] or Codex). Its session persists within
the discovery subworkflow via `durable: true` (`schema.rs:64`).

1. **Parse the phase doc.** Mechanical extraction of question-shapes from
   the phase doc prose. These are structs the scouts consume — not a
   triage decision. Deliberately cheap: a `shell` hook-op
   (`src/workflow/ops.rs:55`) parsing YAML frontmatter or a cheap-LLM
   Executor node before the inlet. No typed frontmatter-op exists today;
   `shell` is sufficient for v1.

2. **Dispatch scouts.** `foreach` over question-shapes
   (`schema.rs:193-276`). Each iteration runs a scout subworkflow —
   corpus-pathfinder agent returning structured leads (tldr,
   entity_refs, path_ids, bundle_handle, gap_check). Scouts are
   parallel-safe, dispatched up to `foreach.parallelism`.
   Results collect into `vars.scout_results` (`engine.rs:1834-1839`).

3. **Read scout results.** The inlet reads the collected scout outcomes.
   Each contains structured exports — not prose the inlet has to re-parse.
   The inlet deduplicates entity refs, resolves conflicting leads, and
   assembles an **evidence manifest**: the exact set of file:line refs,
   entity_refs, and path_ids the implementer must load.

4. **Measure the manifest.** Calls `bbox_ref_size(refs=[...])` — a
   mechanical MCP tool that resolves refs to their byte payload and
   returns the aggregate size. No LLM estimation. No eyeball compaction
   factors. A number.

   Acceptance-coverage lint is not a pure packet gate today. The packet AST can
   quantify over one array path, but cannot correlate `acceptance_criteria[*]`
   against `sub_units[*].acceptance_subset[*]`; use a mechanical hook/tool for
   that coverage check.

5. **Produce the triage verdict.** If the measured manifest fits in the
   target model's context window → `fit_direct`. If it exceeds →
   `needs_decompose`. The inlet also exports the evidence bundle
   (structured JSON of refs + measurements) and, if decomposition is
   indicated, may sketch an initial DAG for the ensemble to start from —
   but the ensemble owns the final DAG.

### 3.2 Why scouts before triage

The predecessor got this backwards. The inlet cannot know whether the work
fits until scouts have found the actual files, symbols, and paths the
implementer will touch. An LLM guessing "estimated read load" from phase
doc prose is confabulation. Scouts find the ground truth. The `ref→size`
tool measures it. The inlet routes on the measurement.

Scouts may reveal the work is trivial (few files, small payload). Or they
may reveal scope was massively underestimated (deep call graphs, many
components). The inlet discovers this from data, not guesswork.

### 3.3 The evidence manifest

The inlet's output to downstream:

```json
{
  "triage_verdict": "fit_direct | needs_decompose",
  "evidence_bundle": {
    "total_bytes": 45120,
    "target_context_window": 200000,
    "refs": [
      {"ref": "project_file:<project_id>:<hash>:<chunk>:0", "bytes": 2140},
      {"ref": "project_file:<project_id>:<hash2>:<chunk2>:0", "bytes": 895}
    ],
    "knowledge_ids": ["kn-..."],
    "path_ids": ["path-..."]
  },
  "dag_sketch": null
}
```

The `bytes` per ref come from the `bbox_ref_size` tool — the tool resolves
each ref and returns its resolved byte size. The inlet sums them and
compares to the model's context window.

### 3.4 Subworkflow boundary

The inlet runs inside a discovery subworkflow (`schema.rs:144-151`). The
subworkflow imports `phase_doc_path` from the parent and exports
`evidence_bundle`, `triage_verdict`, and `dag_sketch` back to the parent
(`engine.rs:2464-2570`).

The discovery subworkflow node carries a `gate` packet
(`schema.rs:120-127`). The gate's entity includes the subworkflow's exported
vars. The gate packet reads `vars.triage_verdict` and emits the verdict
(`fit_direct` or `needs_decompose`) as its classification. The parent's
`Branch` transition (`schema.rs:389-395`) routes on `last_verdict` (the gate
verdict, per `BranchSelector::GateVerdict`). This is the standard gate →
branch routing pattern — no new mechanism needed, just an explicit gate
packet on the discovery node.

## 4. Stage 2: Ensemble decomposition

### 4.1 When it fires

Only when `triage_verdict == needs_decompose`. The parent workflow's
`Branch` routes here.

### 4.2 What it does

A whiteboard deliberation following the `whiteboard-arc.json` pattern
(`examples/whiteboard/workflows/whiteboard-arc.json`):

- **Blind post:** Each panel member posts a proposed decomposition
  independently. Posts are typed structured proposals with target
  files/symbols.
- **Debate:** Members read each other's posts, annotate, vote.
- **Resolve:** Facilitator reads final state, emits the DAG.

The ensemble uses the **same** `bbox_ref_size` MCP tool — but deeply,
cluster-by-cluster. Each proposed sub-unit's file/symbol refs are batched
through the tool to measure the per-cluster payload. This informs the DAG
construction: clusters that fit individually but collectively exceed budget
get split, serialized, or re-clustered.

### 4.3 DAG output

```json
{
  "sub_units": [
    {
      "sub_unit_id": "su-1",
      "refs": ["project_file:...", "symbol:..."],
      "bytes": 42000,
      "acceptance_subset": [
        {"criterion_id": "a1", "criterion_text": "dark mode toggle in settings"}
      ],
      "depends_on": []
    }
  ],
  "recompose_contract": {
    "merge_order": ["su-1", "su-2"],
    "cross_subunit_tests": ["test_integration_seam"],
    "leftover_acceptance_ids": []
  }
}
```

Every parent acceptance criterion must appear in at least one sub-unit's
`acceptance_subset` by stable `criterion_id`. This is mechanically
lintable: after the ensemble produces the DAG, a `shell` hook-op (or a
future typed `lint_acceptance` op) iterates acceptance IDs and verifies
coverage. No typed coverage-lint op exists in the engine today
(`src/workflow/ops.rs:53`); `shell` is sufficient for v1.

### 4.4 Implementer dispatch

`foreach` over DAG sub-units (`schema.rs:193`). Each sub-unit is a
**supervised subworkflow** (`design/archive/supervision.md`): the implementer
runs inside a subworkflow with mechanical telemetry plus optional classifier
and advisor-gated completion. The implementer is seeded with its evidence
subset (the refs from the DAG for that sub-unit). No exploration, no
re-grepping. The evidence was already loaded by scouts and measured by the
inlet.

Results collect via `foreach.collect.into_var` as an array of
`FanoutChildOutcome` objects (`engine.rs:1834-1839`). Each outcome
carries `{status, exports, outputs, arc_id, error}` (`engine.rs:655-665`).
The sub-unit's advisor verdict and acceptance status are in `exports` when
the child subworkflow declares them in `foreach.exports`
(`schema.rs:264-268`).

### 4.5 Recompose council

The recomposition council is a **separate** ensemble from the decomposer
panel. The decomposer produces the DAG (§4.2). The recomposition council
evaluates implementer outputs and decides iterate vs halt. Different
brofiles, different charters. The council is `durable: true`
(`schema.rs:64`) so its session persists across node visits within the
same workflow runner — when the workflow loops back, the same actor is
invoked again with accumulated context. It does NOT persist across a
fresh arc or subworkflow boundary (`engine.rs:600, 720`).

1. **Satisfied?** All parent acceptance criteria met, all sub-unit
   advisors passed, integration seams verified → done. The council emits
   `EXIT_MET`.

2. **Work remains?** Some sub-units failed, or acceptance criteria are
   unmet. The council produces a **remediation packet** — a new phase doc
   describing the remaining work, scoped to what's left, with the prior
   batch's outcomes as context. This packet re-enters the pipeline:
   inlet → decomposer → dispatch → advisor gate → council re-evaluates.
   This loop is a `Goto` inside the same workflow arc, not a fresh arc
   dispatch, so durable council actor context survives across epochs.

3. **Untenable?** Repeated failures on the same sub-unit, impossible
   acceptance criteria, budget exhausted. The council halts with an
   escalation note (`CHARTER_DRIFT` or `halt`, routed to human).

The remediation packet goes through the **same** inlet that handled the
original phase doc. The inlet re-scouts if needed (new code may have been
authored in prior batches), assembles an updated manifest, and routes.
The decomposer may produce a revised DAG. The foreach dispatches new
implementers. The same durable council evaluates the next batch.

This converges iteratively: each epoch reduces the remaining work surface.
If the council can't converge after a configurable epoch ceiling
(`retry.max_generations` or a dedicated `max_epochs` field), it halts.

## 5. Supervision layers (separate infrastructure)

The phase-decomposer pipeline composes supervision - it does not own it.
Each implementer dispatch (single or fan-out) runs inside a supervised
subworkflow defined in `design/archive/supervision.md`: mechanical telemetry
is available for every task; a workflow-backed classifier atom may poll the
primary; an advisor atom may evaluate turn-end or alert checkpoints; verdicts
route to acceptance, steering, recovery, replacement, human escalation, or
bail. N implementers = N advisors when advisor supervision is enabled.

The supervision layers are separate infrastructure. See
`design/archive/supervision.md` for the full specification.

## 6. Fault handling: two distinct paths

### 6.1 Pre-recompose: sub-unit failure

A sub-unit fails (implementer looped, `replace_primary` / `cancel_and_retry`
could not recover, advisor escalated too many times). The foreach collects a
failed outcome. The council reads it. The council produces a **remediation
packet** — a new phase doc scoped to that sub-unit's remaining work — and
pushes it through the inlet → decomposer → dispatch → advisor gate → council
re-evaluates.

Re-dispatch is the first escalator: advisor action `replace_primary` or
`cancel_and_retry` before a remediation packet. The remediation packet is used
when recovery also fails or the advisor declares the sub-unit untenable.

### 6.2 Post-recompose: integration conflict

All sub-units PASSED. Advisors signed off. Individual acceptance criteria
met. The council attempts to merge. The merge fails. This is NOT a
sub-unit failure — nobody failed their individual work. The integration
seam doesn't close.

The council handles this in-place via M1-M4. It does NOT re-enter the
inlet unless mediation exhausts all options.

#### M1 — Mechanical merge

A `shell` hook-op (`src/workflow/ops.rs:55`) runs `git merge` for each
sub-unit branch in merge order. Fast-forward merges succeed silently. If
merge conflicts → M2.

#### M2 — Conflict-resolver agent

An Executor with a resolver brofile. Its prompt includes:
- Merge conflict markers (both sides' versions of the conflicted file)
- Both sub-units' acceptance criteria and predicted writes
- Both sub-units' advisor verdicts and outputs

Its job: produce a **concrete file edit** that integrates both sides.
Commits the resolution. It does NOT drop acceptance criteria — if
satisfying both is impossible, it surfaces the conflict with an explicit
note ("can't satisfy criterion A and criterion B simultaneously because
they touch the same code path differently") and escalates to M3.

The resolver is an implementer, not a judge. Only the council can modify
or drop acceptance criteria.

#### M3 — Mediation panel

Whiteboard deliberation (`examples/whiteboard/workflows/whiteboard-arc.json`).
One advocate per conflicting sub-unit. Each advocate posts:
- Their sub-unit's charter and acceptance criteria
- Why their version should win
- What compromise they're willing to accept

The panel debates. The facilitator reads `whiteboard_summarize` and
emits a resolution — which side wins, a compromise edit, or deadlock.
Commits the resolution.

If the panel deadlocks (no majority, unresolvable conflict) → surfaced
to the council as a failed mediation.

#### M4 — Regression-fixer

After merge resolution: run the project's test suite. If tests fail, an
Executor (fixer brofile) reads the failure output and patches the
failures. Re-runs up to `retry.max_generations` (`schema.rs:349-353`).
If tests still fail after ceiling → surfaced to the council as a failure.

#### Council verdict on mediation

The council reads M1-M4 outcomes:

- **Resolution worked, tests pass** → batch is satisfied. Council may
  declare `EXIT_MET` or proceed to next batch.
- **Mediation deadlocked, or tests won't pass** → council produces a
  **remediation packet**. This packet describes the unresolvable conflict
  and may include modified acceptance criteria (only the council can
  modify criteria). The packet re-enters the inlet → decomposer →
  dispatch → advisor gate → council re-evaluates.
- **Untenable** (same conflict recurs across epochs, budget exhausted) →
  council halts. Human escalation.

The council is the only entity that judges. M2 resolves merges. M3
debates. The council decides.

## 7. Key primitives (grounded)

| Primitive | Location | Status |
|---|---|---|
| Actor kinds (Executor, Ensemble) | `src/workflow/schema.rs` | implemented |
| NodeSpec (prompt, gate, on_exit, wait_for, late_inject, foreach, subworkflow) | `src/workflow/schema.rs` | implemented |
| Foreach fanout | `src/workflow/schema.rs`, `src/workflow/engine.rs` | implemented |
| Subworkflow + imports/exports | `src/workflow/engine.rs` | implemented |
| Branch transition (gate verdict routing) | `src/workflow/schema.rs`, `src/workflow/engine.rs` | implemented |
| Fork (parallel fire-and-forget) | `src/workflow/schema.rs`, `src/workflow/engine.rs` | implemented |
| Wait (signal suspension) | `src/workflow/wait.rs` | implemented |
| Signal dispatch | `src/server/routes.rs` | implemented |
| cancel_task (SIGTERM) | `src/orchestration/mod.rs` | implemented |
| Per-event hook seam | `src/orchestration/mod.rs`, `src/orchestration/supervision.rs` | implemented |
| Whiteboard deliberation | `src/whiteboards.rs`, `examples/whiteboard/` | implemented |
| Policy packet (arc-level gate) | `src/workflow/schema.rs`, `src/workflow/engine.rs` | implemented |
| Compaction anchor (rolling summary) | `src/workflow/engine.rs` | implemented |
| Durable actor sessions | `src/workflow/schema.rs`, `src/workflow/engine.rs` | implemented |
| Agent manifests (typed install artifacts) | `system-defaults/agents/code-reviewer.json` | implemented |
| Advisor checkpoint/packet/resume pipeline | `src/tools/roster.rs` | implemented (team-scoped) |
| Mechanical supervision telemetry | `src/orchestration/supervision.rs` | implemented |
| Classifier workflow-backed atom pattern | `system-defaults/atoms/supervision/classifier.json`, `system-defaults/workflows/supervision/classifier.json`, `src/tools/atoms.rs` | implemented |
| Advisor workflow-backed atom pattern | `system-defaults/atoms/supervision/advisor.json`, `system-defaults/workflows/supervision/advisor.json`, `src/tools/atoms.rs` | implemented |
| `bbox_ref_size` MCP tool (ref→bytes measurement) | — | **aspirational** |
| Typed advisor action executor | `src/tools/atoms.rs` | implemented |
| Mediation agent manifests | — | **aspirational** |

## 8. What this design does NOT do

- **No preload-as-separate-rung.** The inlet assembles the manifest and
  measures it. There is no phantom middle tier between scouts and
  implementer.
- **No triage before scouts.** The inlet runs scouts first, then produces
  the verdict. Decisions are informed by data, not guesses.
- **No interception of provider built-ins.** The coercion surface is
  described in `design/workspace-tools.md`. Hard interception requires a
  parallel mechanism per provider — not wired today
  (`src/orchestration/providers.rs:790-850`).
- **No overminds or scope_expansion_request.** These are daystrom design
  artifacts (`../daystrom-mk2/design/dispatch-v2.md:5` — "Not yet
  implemented"). Bbox does not have them and this design does not depend
  on them.
- **No new actor kinds.** The engine has two (`Executor`, `Ensemble`).
  Roles are workflow-author concerns expressed via brofile + prompt +
  `on_exit` parse_json + gate.

## 9. Build sequence

1. **`bbox_ref_size` MCP tool.** Resolves entity_refs/project_file_refs to
   byte payloads. The shared measurement primitive both stages depend on.
2. **Scout agent manifest.** Corpus-pathfinder as installed JSON agent
   (`examples/agents/`). Strict-typed structured output. Parallel-safe.
3. **Inlet agent.** The discovery subworkflow that orchestrates scouts,
   aggregates results, calls `bbox_ref_size`, and produces the triage
   verdict + evidence bundle.
4. **Single-implementer path** (fit_direct). Seeded with exact manifest
   from inlet. Smallest viable pipeline end-to-end.
5. **Ensemble decomposition** (needs_decompose). Whiteboard deliberation
   producing DAG, using `bbox_ref_size` cluster-by-cluster.
6. **Implementer foreach** over DAG sub-units, each in a supervised subworkflow.
7. **Recompose council** — durable ensemble evaluating collected results, producing remediation packets, iterating until satisfied or untenable.
8. **Mediation** — M1-M4 within the council's evaluation loop (mechanical merge, conflict resolver, mediation panel, regression fixer).

Each step independently testable. Steps 1-3 deliver value before any
decomposition machinery exists.

# ARCHIVED — Phase Decomposer (v0, predecessor-authored)

**Archived:** 2026-05-10
**Originating session:** Claude `04e4025e-792e-4283-8924-034a1984b341`
**Review session:** Codex `019e12d1-a673-7913-b191-9ea94a2ecc74`
**Thread:** `thread-ffe3c075`

This document was authored by a predecessor agent that repeatedly proposed
architectures without grounding in code. The user lost trust and abandoned the
session. Several claims in this doc are incorrect or aspirational — see the
handoff doc at `HANDOFF.md` for a full audit of what was confabulated.

Archived for provenance. The replacement is
`design/partial/phase-decomposer.md`.

--- BEGIN ORIGINAL ---

# Phase Decomposer — context-budget-aware execution of large phased plans

Date: 2026-05-10
Status: design proposal — incorporates codex review findings (2026-05-10);
depends on `design/workspace-tools.md` for instrumentation and coercion.

## 1. Problem

Some bro providers compact catastrophically. Codex handles compaction well;
others don't. The signature failure mode is **vibe bailing 80% through a
phase**: the agent burned its context window on exploration and re-reading
through a phase doc, then compacted, then dropped intent. The phase didn't
finish. The committed work is a partial, unverified slice of the acceptance
criteria.

The naive responses don't generalize:

- *Route around bad providers.* Loses the providers' other strengths;
  doesn't help when even strong providers face genuinely large phases.
- *Smaller phases.* Pushes the decomposition burden onto the human author
  of the plan; doesn't help when phases are written by a third party (an
  upstream design doc, a roadmap, a customer spec).

The general-purpose move is to **fit the work to the budget mechanically**:
measure the budget, predict the load, and either (a) preload aggressively to
shrink runtime exploration, or (b) decompose the phase into sub-units that
each fit, with a DAG, contention prediction, and a recompose contract.

## 2. Thesis

The infrastructure to do this is *partially* available. Some pieces are
implemented and shippable; some are donor *patterns* we'd be implementing,
not pre-built parts:

- **Implemented and reusable today:**
  Keystone-style workflow engine (`examples/keystone/`), whiteboards
  (`examples/whiteboard/`, in-engine `whiteboard_*` tools), work-item threads
  (`bbox_thread kind=work_item`), structured notes (`bbox_note`), ambient
  scope injection (`apply_ambient` in `src/orchestration/mod.rs:493-670`),
  surface scoping (`bro_exec surface=`).
- **Implemented in daystrom, ports needed:**
  Workspace tools — file/git/shell wrappers with augmentation
  (`../daystrom-mk2/src/Daystrom.Worker/Tools/`). These are the
  instrumentation substrate and ship in `design/workspace-tools.md`.
- **Donor patterns, design-stage even in daystrom:**
  Dispatch-v2's overminds, write-target DAG, scope-expansion-request,
  signal processors. `../daystrom-mk2/design/dispatch-v2.md:5` says
  "Not yet implemented"; `work-order-process.md:399` says overminds are
  disabled for normal dispatch. We'd be **implementing the pattern**, not
  reusing existing wiring.

What's genuinely new here:

1. **Tree-sitter symbol-set as `predicted_writes`** — daystrom uses files;
   we want symbols. `bbox_code_symbols` returns `semantic_status="syntax_only"`
   (`src/code_nav/mod.rs:657`), so symbol-set predictions are best-effort
   without LSP integration. Phase-decomposer treats them as **hypotheses
   to falsify by instrumentation**, not authoritative graphs.
2. **Scout-as-preloader contract.** A scout whose deliverable is a *context
   bundle* pinned to the implementer's work-item. Converts uncertain future
   read load into pre-paid measured cost.
3. **Mediator overmind** that runs in LIVE mode. Implementing daystrom's
   pattern, not reusing it. Project-scoped memory is **deferred** until
   contention data exists worth memorizing.

## 3. Three rungs

Decomposition is the fallback, not the default. Many phases that would
otherwise compact are saved by preload alone.

### 3.1 Triage gate

Cheap. Computes:

- `nominal_context(provider, model)` — from the provider catalog.
- `effective_budget = nominal_context * compaction_factor[provider]` —
  per-provider quality factor calibrated from `tool_call` events
  (`design/workspace-tools.md` §5) cross-referenced with task outcomes.
  Codex factor ≈ 0.9; Opus 4.7 factor ≈ 0.6 (eyeball). **Actual numbers
  require the workspace-tools instrumentation pipeline to land first**;
  until then, eyeball factors gate the rung.
- `predicted_load = phase_doc_size + ambient_overhead + estimated_read_load`.
  Estimated read load is rough: count of cross-references, depth of call
  graph at the predicted entry points, BLACKBOX.md size for the project.

If `predicted_load < effective_budget * safety_margin`, dispatch direct.
No scouts, no council, no decomposition. Most small phases land here.

### 3.2 Preload-only

Triage says load is on the edge but not over. Run the scout pre-flight
(§5), pin the context bundle to the implementer's work-item, dispatch.
The implementer's ambient scope block now references the pin so it never
re-greps what the scout already loaded.

This is the rung that addresses the vibe-at-80% case without any
decomposition machinery.

### 3.3 Decomposition

Triage says load exceeds budget even with preload, OR the phase declares
it spans multiple components, OR a prior dispatch on this phase already
failed mid-way. Run the full pipeline (§4–§9).

### 3.4 Structural escape

Even decomposed sub-units don't fit. Recurse decomposition once
(max-depth 2 total levels); if still over, surface to human for
replanning. Don't loop further.

## 4. Pipeline shape

Shipped as a workflow artifact `examples/phase-decompose/workflows/phase-decompose.json`,
parallel to keystone's `issue-to-merged-pr.json`. Distributed via
`bbox_artifact_install kind=workflow`. Per-project tuning via packets
(`domain:phase-decompose/budget`, `domain:phase-decompose/granularity`,
`domain:phase-decompose/mediator-policy`).

```
                  ┌─────────────────────┐
                  │ Phase ingestion     │  input: phase_doc, work_item_id,
                  │ (workflow start)    │         prior_progress (optional)
                  └─────────┬───────────┘
                            │
                  ┌─────────▼───────────┐
                  │ Triage gate         │
                  │ packet: budget      │
                  └─────────┬───────────┘
                  ┌─────────┼───────────┐
       direct────►│         │           │◄────decompose
                  ▼         ▼           ▼
              Implementer  Preload   Scout fan-out
                           +impl     (workflow foreach)
                                      │
                                      ▼
                                  Whiteboard
                                  (decomposer council)
                                      │
                                      ▼
                                 DAG emit + per-unit
                                 AssignmentPackets
                                      │
                                      ▼
                              Sub-unit dispatch
                              (parallel / serial per DAG)
                                      │
                                      ▼
                                  Recompose
                              (acceptance verification,
                               integration, completion)
```

Fan-out and fan-in primitives are the workflow engine's existing
`foreach` / `wait_for` (`src/workflow/engine.rs:1235`,
`src/workflow/schema.rs:193`) — not hand-rolled `Wait` nodes
on `scout-complete` ×N.

## 5. Scout pre-flight

### Contract

Input:

- Phase doc (parsed; see §10 on shape).
- Project root.
- Optional: prior_progress diff + done-notes (resume case).

Output: a strict-typed `ContextBundle` (JSON):

```
{
  "refs": [{ "path": "...", "lines": [start, end] }, ...],
  "symbol_hypotheses": [
    {
      "symbol_id": "symbol:rust:foo::bar",
      "confidence": "high" | "medium" | "low",
      "uncertainty_notes": "..."
    }, ...
  ],
  "knowledge_ids": ["sm-...", "kn-..."],
  "preamble": "I've already loaded the following ..."
}
```

Strict-typed JSON, not prose-with-anchors. The required `confidence` and
`uncertainty_notes` fields force the scout to surface what it's unsure
about; downstream consumers (council, mediator) can de-rate low-confidence
hypotheses.

Stored as a `bbox_pin(scope=bro, target=<implementer_id>)` so the ambient
scope block delivers it on every implementer turn without re-fetch.

### Caveat: symbol resolution is syntax-only

`bbox_code_symbols` returns syntax inventory with
`semantic_status="syntax_only"` (`src/code_nav/mod.rs:657`). Refactor docs
explicitly say code-nav tools are syntax locators, not binding/reference
resolvers (`src/system_memory/refactor.md:16`). So
`symbol_hypotheses` are syntactic guesses, not semantic graphs. Two
implications:

- Same-name symbol shadowing across modules can produce false collisions
  in the contention check (§7).
- Edits to a symbol's *call sites* aren't predictable from the symbol set
  alone; they require LSP-driven reference resolution we don't have.

The mediator (§8) compensates by treating symbol-level contention as a
**hypothesis**, not authoritative — it falls back to file-level when
symbol resolution is uncertain (`confidence: low`).

### How scouts run

Dispatched via the workflow engine's `foreach` primitive
(`src/workflow/schema.rs:193`), one scout per proposed sub-unit. Their
results join via `wait_for` (`src/workflow/engine.rs:1235`).

Scouts run the standard agentic opening sequence
(`sm-agentic-opening-sequence`): describe-schema → hybrid-search →
inspect-entity → find-paths, plus targeted `bbox_code_symbols` and
`bbox_code_node_describe` for the projection from prose-intent to code
structure.

Provider routing: scouts can run on cheaper providers (Haiku, Sonnet,
Codex-medium); their output quality matters but their context budget is
narrower than the council's.

### The hard part

The scout's load-bearing inference is *projecting prose intent onto code
structure*. Tree-sitter alone can't do this — the LLM has to read the
phase doc and emit "this sub-unit will touch these symbols." The quality
of that projection sets a ceiling on DAG quality. **This is where to
spend the iteration budget.**

## 6. Decomposer council (whiteboard)

Phase-gated deliberation, not free-form chat. Members: Opus 4.7 [1M] +
Codex (large-context providers; they hold the entire phase doc + all
scout reports without compacting). Optional third voice for tie-breaking
(Sonnet 4.6 or another Codex variant).

### Phases (whiteboard lifecycle)

- **blind**: each council member posts a proposed decomposition
  independently. Posts are typed `proposal` with `target_file`,
  `target_location`, and a `claim` body containing the proposed
  sub-units.
- **debate**: members read each other's posts, raise concerns
  (`whiteboard_post kind=concern`), annotate, vote.
- **resolve**: facilitator (the workflow engine, mechanical) reads final
  state. If `whiteboard_conflicts` reports unresolved conflicts above a
  severity threshold, escalate (re-run debate, or surface to human).
  Otherwise emit the synthesized DAG artifact.

### Conflict detection

`whiteboard_conflicts` (`src/whiteboards.rs:351-365`) detects:

- `direct_overlap`: same `target_file` + identical `target_location`.
- `cascade_collision`: post A's `cascade_targets` include post B's
  `target_file`.
- `severity_disagreement`: same `finding_ref`, different severity.

**Important caveat:** `direct_overlap` is keyed on
`target_file`+`target_location` *strings*, not on a symbol-DAG check. To
catch decomposition disagreements at symbol granularity, council members
must encode symbol IDs into `target_location` by convention. The council
brofile lens enforces this; without it, conflicts are file-level only.
This is a real constraint, not free.

### Output (DAG artifact)

```
{
  "phase_id": "...",
  "sub_units": [
    {
      "sub_unit_id": "...",
      "predicted_writes": ["symbol:rust:foo::bar", "symbol:rust:baz::qux"],
      "predicted_reads": ["..."],
      "preload_pin_id": "pin-...",
      "acceptance_subset": [
        { "criterion_id": "<from parent>", "criterion_text": "..." }
      ],
      "depends_on": ["sub_unit_id_..."]
    },
    ...
  ],
  "recompose_contract": {
    "merge_order": ["sub_unit_id_..."],
    "cross_subunit_tests": ["..."],
    "seam_closure_checks": ["..."],
    "leftover_acceptance_ids": []
  }
}
```

### Acceptance projection

The council's job for each sub-unit's `acceptance_subset` is **LLM-emitted
projection plus mechanical lint**: every parent acceptance bullet must
appear in at least one sub-unit's `acceptance_subset` by stable
`criterion_id`. The lint runs after the resolve phase; if it fails, the
decomposition is incomplete and the council re-runs debate.

This makes recompose verifiable (§9). Without stable criterion IDs and
the lint, sub-units silently lose acceptance bullets.

## 7. Sub-unit dispatch + scope expansion

Each sub-unit dispatches as a standard `bro_exec` with:

- The `AssignmentPacket` (predicted_writes + predicted_reads +
  acceptance_subset + depends_on) injected via the ambient scope block
  (`apply_ambient` extension point, `src/orchestration/mod.rs:493-670`).
- The `ContextBundle` referenced via `bbox_pin`.
- The sub-unit work-item thread (child of the parent phase thread).
- `coerce_workspace=true` — emits the workspace-tools appendix
  (`design/workspace-tools.md` §6), which lists `bbox_smart_read`,
  `bbox_bash`, `bbox_git_*` as preferred tools.

### Scheduling

Topological order from the DAG. Disjoint sub-units (no symbol overlap in
predicted_writes) run concurrently. Overlapping sub-units serialize.
This is the cheap part — it kills 90% of the contention surface before
any agent runs.

### Scope expansion — instrumentation, not interception

Provider built-ins (`Edit`, `Write`, `Bash`) are outside the MCP catalog;
the existing filter stack can't disable them
(`src/orchestration/providers.rs:790-850`). So **scope expansion is a
post-hoc instrumentation signal**, not a synchronous tool-call
interception:

1. The implementer writes — via `bbox_apply` (instrumented), via
   `bbox_refactor_apply` (already instrumented), or via raw `Edit` /
   `Write` (logged as `tool_call_event` per `design/workspace-tools.md` §5).
2. A scope-check signal processor runs on each `tool_call_event` write:
   if the `tool_target` (file path, or symbol when resolvable) is outside
   `predicted_writes`, it emits
   `bbox_note(kind=dispute, body="reach outside predicted_writes:
   <symbol_or_file>", thread_id=<sub_unit>, task_id=<dispatch>)`.
3. The mediator (§8) reads disputes at round boundaries.

Two routing cases:

- *Disjoint reach* (no sibling sub-unit has this target predicted):
  re-DAG; treat as a low-cost amendment to the predicted_writes set. No
  cancellation.
- *Sibling overlap* (an active sibling sub-unit has this target
  predicted): route to mediator (§8).

This is daystrom's **scope_expansion_request pattern**, but reformulated
around instrumented tool calls instead of an executor-called MCP tool
(daystrom's design at `dispatch-v2.md:952` and `dispatch-v2.md:1943`).
The reformulation is forced by bbox's lack of a native interception
surface; the trade-off is that violations are visible, not blocked.

### Tree-sitter symbol granularity wrinkle

Edits to *existing* symbols are predictable from syntax inventory. Edits
that *add* new symbols can't be predicted; they collide only on rare
same-name additions. Cheap rule: predicted_writes is a set of symbol
IDs; runtime symbol-add is non-conflicting unless it shadows.

When `confidence` on a symbol hypothesis is `low` (§5), the contention
check falls back to file-level: the dispute fires if any write touches
the file, not just a specific symbol. False-positive cost; acceptable in
exchange for not missing real conflicts when symbol resolution is shaky.

## 8. Mediator overmind

Lightweight overmind, not a council. When two implementers contend on a
target (symbol or file), the mediator runs in **LIVE mode** (daystrom's
three-path pattern from `dispatch-v2.md` §6.3) and `session.SendAsync()`
injects into both implementers without canceling. Cancel-and-resume is
the fallback when injection doesn't resolve.

Daystrom itself recommends *scheduler-mediated coordination first, direct
agent-to-agent messaging later* (`dispatch-v2.md:2182`). Mirror that:
the mediator's first move is reading both sides' disputes and
`assumption` notes, surfacing the disagreement structurally, and asking
the scheduler to serialize the contended sub-units. Direct injection is
escalation, not default.

### Required substrate: structured intent capture

Implementers should emit `bbox_note(kind=assumption, body="reaching for
Foo::bar because X")` whenever they touch outside their predicted_writes
set. **Soft nudge** — the workspace-tools coercion appendix mentions it,
but enforcement isn't possible (provider built-ins fall outside MCP
interception). Hard rejection only becomes valid once writes are
required to flow through interceptable MCP/refactor tools, which is a
follow-on to workspace-tools §6.3 v3.

The mediator's first move is reading both sides' assumption notes and
surfacing the actual disagreement, not generating a fresh debate from
scratch. When notes are absent, the mediator works from the
`tool_call_event` records directly.

### Project-scoped contention memory — deferred

Earlier draft proposed a per-project mediator overmind that accumulates
recurring contention patterns across phases. **Deferred from v1.**
Codex review correctly flagged this as adding identity / persistence /
retrieval / prior-injection machinery before there's contention data
worth memorizing. Notes and `tool_call_event` records are sufficient
until repeated patterns prove the memory earns its prompt budget.

The upgrade path: once `tool_call_event` shows recurring contention on
specific files/symbols across multiple phases, promote the pattern to a
standing `bbox_pin` on the project; the mediator reads pins on
summoning. No new infrastructure; just a new pin-write site.

## 9. Recompose + acceptance verification

Recompose is **not an afterthought**. It defines what a *valid
decomposition* is. If recompose can't satisfy the parent acceptance
criteria, the decomposition was wrong and v1 should treat that as a
first-class failure mode.

The recompose step is an **explicit integration unit**, not just a
verification gate:

1. **Merge order** — sub-units land in DAG topological order. Each
   merge runs the build/test gate, not just the final merge.
2. **Cross-subunit tests** — tests that exercise the integration seams
   between sub-units. Authored as part of the decomposition (the council
   emits them in `recompose_contract.cross_subunit_tests`); run after
   each sub-unit merge that closes a seam.
3. **Acceptance subset re-injection** — at the build/completion gate
   for each sub-unit, re-inject its `acceptance_subset` as a verification
   checklist (daystrom's pattern, `../daystrom-mk2/design/steering.md:602`).
   Bbox workflow gates are packet-based today, not build-gate equivalent;
   either ship a workspace-tools-driven build gate or accept that
   acceptance verification is advisory until the harness layer matures.
4. **Leftover acceptance check** — after all sub-units merge, the lint
   from §6 runs again: every parent criterion ID must appear in at
   least one merged sub-unit's `acceptance_subset` *and* be marked
   verified by that sub-unit's gate. Unverified IDs land in
   `recompose_contract.leftover_acceptance_ids` and surface to human.
5. **Seam closure verdict** — explicit pass/fail on whether the
   integration seams between sub-units are closed. Open seams are
   surfaced; the framework doesn't try to auto-close them.

Failure modes the recompose step must catch:

- Sub-unit drift: an implementer satisfies its `acceptance_subset` but
  introduces a regression in a sibling sub-unit's predicted_reads.
  Caught by cross-subunit tests + post-merge gate.
- Acceptance loss: a parent criterion has no sub-unit projecting it.
  Caught by §6 lint *and* the post-merge re-lint.
- Seam open: sub-units land but don't compose. Caught by explicit seam
  closure checks; falls back to human if auto-detection fails.

## 10. Phase-doc shape

Phase docs are heterogeneous markdown today. The decomposer's first job
is structured extraction every time, which is wasteful and unreliable.

**Position: parser plug-in per project, with a recommended frontmatter
schema shipped in this repo as a default.** Bbox doesn't own every
project's phase-doc convention; projects ship their own parser when their
docs deviate, and the workflow accepts a `phase_doc_parser_id` pointing
at a packet or a thin extraction tool.

Recommended default frontmatter:

```yaml
---
phase_id: ...
depends_on: [...]
acceptance:
  - id: a1
    text: "criterion 1"
  - id: a2
    text: "criterion 2"
scope_hints:
  components: [...]
  files: [...]   # optional, soft hint
---

# Phase 3: Migrate auth to JWT

[prose...]
```

Stable `id` per acceptance criterion is required for the lint in §6 to
work. Phase docs without frontmatter still flow through the pipeline —
the council's first action becomes structured extraction from prose,
paying the parse cost every dispatch.

## 11. Resume from partial failure

The vibe-at-80% case. Substrate is already there:

- Phase work-item thread carries notes from prior partial-execution.
- Decomposer council reads `bbox_notes(thread_id=phase_id, full=true)`
  during its blind phase.
- Implementers commit in observable chunks. The natural milestone
  granularity is **per logical sub-step tied to acceptance IDs and
  observable validation** — not per symbol (too noisy), not per full
  criterion (too coarse). When the implementer uses
  `bbox_git_commit` (`design/workspace-tools.md` §4.2), each commit
  auto-emits `bbox_note(kind=done, body="commit <sha>: <criterion_ids>
  verified")` as a side effect; manual done-notes are the fallback when
  the implementer doesn't use the wrapper.
- Decomposer's input is `(phase_doc, current_progress_diff,
  remaining_acceptance_criteria)`, not just `(phase_doc)`.

## 12. Donor-primitive map

Audit per codex review (2026-05-10):

| Decomposer pipeline step | Donor primitive | Status |
|---|---|---|
| Phase-doc ingestion + start signal | Keystone-style workflow | implemented in bbox |
| Scout pre-flight, parallel | Workflow `foreach` + `wait_for` (`src/workflow/engine.rs:1235`, `src/workflow/schema.rs:193`) | implemented |
| Decomposer council deliberation | Whiteboard (`whiteboard_*` MCP) | implemented |
| Conflict among proposed splits | `whiteboard_conflicts` (`src/whiteboards.rs:351-365`) | implemented; **caveat: file/location keyed, not symbol-DAG. Council must encode symbol IDs by convention.** |
| Per sub-unit AssignmentPacket | Daystrom dispatch-v2 packet shape (`../daystrom-mk2/design/dispatch-v2.md:896`) | **donor pattern, not implemented in daystrom either.** Field set differs from daystrom (acceptance_subset + preload_pin_id are bbox additions) |
| Sub-unit dispatch | `bro_exec` + `bbox_pin` | implemented |
| Scope expansion | `tool_call_event` instrumentation (`design/workspace-tools.md` §5) → dispute note | **bbox-specific; not interception. Daystrom's design proposes an MCP tool; we use post-hoc telemetry.** |
| Mediator on live contention | Daystrom overmind LIVE mode (`dispatch-v2.md` §6.3) | **donor pattern; daystrom itself recommends scheduler-mediated coordination first** (`dispatch-v2.md:2182`) |
| Completion verification | Acceptance re-injection at gate (daystrom `steering.md:602`) | **donor pattern; bbox workflow gates are packet-based, not build-gate-equivalent.** Workspace-tools layer required for full parity |
| Resume from 80% | Work-item thread notes + `bbox_notes` | implemented; granularity convention (§11) is new |

## 13. Open questions (positions taken; codex review absorbed)

1. **Scout prompt contract** — strict-typed JSON with required
   `symbol_hypotheses[].confidence` and `uncertainty_notes`. Prose
   anchors are not the primary artifact. (§5)
2. **Recurse vs. escape** — recurse once (max-depth 2 total), then
   structural escape. (§3.4, §6)
3. **Assumption-note enforcement** — soft nudge by default. Hard
   rejection requires writes to flow through interceptable MCP/refactor
   tools, which is a workspace-tools v3 concern. (§7, §8)
4. **Acceptance projection** — LLM-emitted with mechanical lint by
   stable criterion ID. Every parent criterion must appear in at least
   one sub-unit's `acceptance_subset`. (§6, §9)
5. **Phase-doc shape** — parser plug-in per project, recommended
   frontmatter schema shipped here as default. Stable acceptance IDs
   required for the lint. (§10)
6. **Done-note granularity** — per logical sub-step tied to acceptance
   IDs. `bbox_git_commit` auto-emits done notes; manual fallback. (§11)
7. **Compaction-factor calibration** — mine `tool_call_event`
   (`design/workspace-tools.md` §5) cross-referenced with task outcomes,
   per provider+model+effort tier. Eyeball factors gate the rung until
   instrumentation lands. (§3.1)
8. **Mediator-memory bootstrap** — deferred from v1. Promote to project
   pins after recurring contention patterns appear in the
   `tool_call_event` log. (§8)
9. **Recompose contract** — explicit integration unit: merge order,
   cross-subunit tests, leftover acceptance IDs, seam closure verdict.
   Not an afterthought. (§9)
10. **Interception surface** — there isn't one. Workspace-tools layer
    provides instrumentation (`tool_call_event`); enforcement is
    advisory via dispute notes + scheduler serialization. Hard
    interception requires routing all writes through MCP/refactor tools
    *and* a parallel `disallowed_builtin_tools` mechanism per provider
    (`src/orchestration/providers.rs:790-850` — not wired today). Out of
    scope for v1. (§7)

## 14. Out of scope

- Replacing `bro_exec` / `bro_resume`. The dispatch primitive is unchanged.
- Replacing existing dispatch flows for small tasks. Triage gate skips
  the whole pipeline for them.
- Reactive merge-conflict resolution. The proactive layer reduces the
  surface; downstream merge tooling still applies.
- New provider integrations. Works with the existing catalog.
- Hard interception of provider built-ins. Workspace-tools instrumentation
  + opt-in coercion + dispute notes is the v1 ceiling.

## 15. Build sequence (revised per codex review)

Codex flagged that "compaction-factor research" was wrong as step 1 —
the load-bearing first step is the typed contract layer. Reordered:

1. **Typed contracts.** Define `PhaseDoc`, `ContextBundle`,
   `AssignmentPacket`, `recompose_contract`, with stable JSON schemas.
   No engine work yet.
2. **Workspace-tools instrumentation.** Schema bump for `tool_call`
   doc-type; parser changes (`design/workspace-tools.md` §5 steps 1–2).
   Now `tool_call_event` is queryable; predicted-writes contention
   checks have a substrate.
3. **Tree-sitter symbol-set as `predicted_writes`** — wire
   `bbox_code_symbols` output into the `AssignmentPacket` shape. The
   contention overlap check is a set intersection over symbol IDs, with
   file-level fallback when `confidence` is `low`.
4. **Compaction-factor calibration.** Mine `tool_call_event` cross-referenced
   with task outcomes; emit per provider+model+effort factors into
   the provider catalog (extend `ModelInfo` in
   `src/orchestration/providers.rs:1823` with budget-related fields).
5. **Scout-as-preloader.** Brofile + manifest for the scout role;
   `ContextBundle` storage via `bbox_pin`; coercion appendix points at
   `bbox_smart_read` so scouts use enriched reads.
6. **Workflow artifact for the preload-only rung (§3.2).** Smallest
   viable pipeline. Exercise it on a real phase doc.
7. **Whiteboard-driven decomposer council.** Standalone test first;
   then wire into the workflow.
8. **Sub-unit dispatch** with scope-expansion as `tool_call_event` →
   dispute note (§7).
9. **Mediator overmind** (LIVE-mode-capable; no project memory).
10. **Recompose + acceptance verification** as an explicit integration
    unit (§9). Cross-subunit tests, seam closure, leftover-ID lint.
11. (Future, deferred) **Mediator-memory accumulation** as project
    pins, when contention patterns warrant.

Each step is independently testable. Steps 1–4 deliver value before any
decomposition machinery exists.

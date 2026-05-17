---
title: "Performance Pathology"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
  - performance
  - pathology
date: 2026-05-16
status: "proposal, awaiting review"
brief: "Diagnosis workflow for performance and efficiency smells, sibling to [[Architecture Pathology]]. Reuses the inlet/ensemble/plan-doc/PD-dispatch machinery and the three-tier smell taxonomy; adds a cost-dimension axis (time/memory/io/network), multi-source evidence (static + profile + query log + metrics) with cross-source corroboration weighting, baseline measurements in the plan-doc, and delta-based acceptance predicates. v0 ships the Java language pack and the ORM-with-lazy-loading + async/await paradigm packs."
---

# Performance Pathology

This design proposes a sibling to [Architecture Pathology](arch-pathology.md)
targeting performance and efficiency smells. The workflow shape, plan-doc +
sidecar artifact contract, operator review gate, three-tier smell taxonomy
(universal / paradigm-bounded / language-idiosyncratic), and PD-dispatch
handoff are inherited unchanged. The new design extends the artifact
schema along four axes specific to performance work: cost dimensions,
multi-source evidence with cross-source corroboration weighting, baseline
measurements, and delta-based acceptance predicates.

The deliverable is a set of example artifacts — detector packets, decomposer
brofiles, baseline-suite templates, plan-doc and sidecar schemas, a glue
workflow — that operators copy into a target project and adapt under
`<project>/design/refactor/perf/`. Architecture and performance pathology
artifacts coexist under `<project>/design/refactor/`; the `perf/` subdirectory
is the convention for separation.

## Motivation

Performance debt is typically tracked as either tickets ("the dashboard is
slow") or hand-rolled benchmark scripts that drift out of sync with the code
they measure. The intermediate state — a structured, reviewable plan of
specific performance findings with measured baselines, target deltas, and
acceptance criteria — does not exist as a first-class artifact in most
projects. Operators end up doing one-off performance audits whose outputs are
slack threads and stale spreadsheets.

Architecture Pathology fills the analogous gap on the structural side. The
machinery there ports cleanly: corpus-pathfinder for scouting hot paths, a
parallel detector battery for evidence collection, an ensemble-validate phase
producing a plan-doc, and an operator review gate before phase-decompose
executes. What changes is the lens, the evidence shape, and the acceptance
contract.

## Scope and non-goals

In scope:

- Mechanized detection of a closed v0 set of performance smells across three
  tiers (universal: nested iteration, loop-invariant recomputation,
  cartesian explosion, redundant serialization, unbounded growth;
  paradigm-bounded: n+1, sequential awaitables, sync IO in hot path,
  missing covering index, eager materialization).
- Multi-source evidence schema covering static analysis, profile samples,
  query logs, and runtime metrics, with cross-source corroboration weighting.
- Baseline measurement capture in the findings sidecar; delta-based
  acceptance predicates that PD's recompose can evaluate.
- Plan-doc lifecycle that accommodates ratcheting baselines (`partial →
  re-plan` as the common terminal state rather than `satisfied → archived`).

Out of scope for v0:

- Automated baseline capture. v0 ships an example baseline-suite template;
  operators run it manually or wire it into their existing CI/perf
  infrastructure. Automated capture as a workflow phase is a follow-up.
- Profile parsers and query-log adapters for arbitrary tooling. v0 ships
  evidence schemas; operators paste in profile excerpts and query-log
  attributions. Source-specific ingest adapters (perf, py-spy, async-profiler,
  RDS Performance Insights, Datadog, etc.) are follow-up designs.
- Continuous monitoring / regression gating in CI. The same reason given in
  Architecture Pathology applies — gating before precision stabilizes
  collapses signal.
- Workload-aware detection. v0 detectors assume the hot path is the request
  handler or per-row callback. Detection conditioned on actual workload
  shapes (read-heavy vs write-heavy, batch vs streaming) is future work.

## Relationship to Architecture Pathology

Shared, ported unchanged from [[Architecture Pathology]]:

- Workflow skeleton: inlet → detector battery → ensemble validates / clusters
  / orders / cuts / writes acceptance → emit plan-doc → operator review gate
  → `phase-decompose-main-edit` consumes.
- Plan-doc + companion findings sidecar artifact contract.
- Deterministic `FND-*` ID scheme.
- Three-tier smell taxonomy (universal / paradigm-bounded /
  language-idiosyncratic) and packet schema with `tier`,
  `implements_universal`, `paradigm_prerequisites`.
- Per-project adoption recipe and `languages:` declaration in the project
  refactor hub.
- Mandatory operator review (`reviewed_by` + `approved_at` frontmatter
  enforced by PD discovery).
- Detector brofiles per-contract rather than per-language.

Specific to performance pathology, added by this design:

- `cost_dimension:` axis on packets and findings (`time | memory | io |
  network`, occasionally `power`).
- Multi-source evidence schema with `sources:` array and corroboration
  weighting.
- `baseline_measurements:` block in the findings sidecar.
- Delta-based acceptance predicates rather than boolean re-detection.
- Plan-doc lifecycle gains an `in-flight (ratcheting)` state for plans whose
  baseline is expected to move with each remediation rather than terminate
  in `satisfied`.

## Architectural shape

Two workflows and one artifact contract, mirroring Architecture Pathology:

```
perf-triage workflow
  inlet phase ......... corpus-pathfinder scouts on operator-named hot
                         paths; layer-model resolve (reused, optional);
                         baseline measurements loaded from
                         <project>/design/refactor/perf/baselines/ or
                         operator-supplied
  detector phase ...... parallel fan-out, one brofile per smell contract,
                         each loads relevant language + paradigm packets
  ensemble phase ...... validates evidence with cross-source corroboration,
                         clusters correlated findings, orders by
                         blocked_by, bounds slice size, cuts to top-N by
                         (corroboration_score × cost_dimension_weight) /
                         blast_radius, writes per-slice delta acceptance
                         criteria
  emit phase .......... renders plan-doc markdown + writes findings sidecar
                         to <project>/design/refactor/perf/plans/<slug>/

[operator review gate — same enforcement as arch-pathology]

phase-decompose-main-edit (existing, unmodified)
  consumes plan-doc as phase_doc_text. PD's discovery phase recognizes
  kind: perf-plan and routes delta acceptance predicates to a
  perf-aware verifier (see Risks).
```

## Artifact contract extensions

The base contract (plan-doc frontmatter, sidecar structure, finding ID
scheme, layer model reference) is inherited from
[[Architecture Pathology]]. The extensions below add to that contract; they
do not replace it.

### Plan-doc frontmatter additions

```yaml
---
title: "Performance Plan: <scope> — <date>"
kind: perf-plan                       # distinct from refactor-plan
lifecycle: proposed | in-flight | in-flight-ratcheting | partial | satisfied | archived
cost_dimensions_in_scope:             # operator declares what's being optimized
  - time
  - io
baseline_capture:
  method: manual | benchmark_suite | profile_sample | query_log_replay
  artifact_ref: design/refactor/perf/baselines/<slug>.yaml
  captured_at: <iso8601>
  captured_against_commit: <sha>
findings_sidecar: <plan-slug>.findings.yaml
---
```

The `in-flight-ratcheting` lifecycle state is performance-specific. It marks
plans expected to terminate in `partial` and trigger a re-plan against a new
baseline, rather than reaching `satisfied`. The state itself is operational
metadata; PD treats it identically to `in-flight`.

### Findings sidecar extensions

```yaml
plan_slug: <slug>
baseline_commit: <full-sha>
baseline_measurements:                # operator-supplied or workflow-captured
  - id: BM-001
    metric: queries_per_request
    scope: src/handlers/order_handler.py:create_order
    value: 47
    unit: count
    captured_via: query_log_replay
    captured_at: <iso8601>
  - id: BM-002
    metric: p99_ms
    scope: src/handlers/order_handler.py:create_order
    value: 840
    unit: ms
    captured_via: benchmark_suite

findings:
  - id: FND-a3f12c4d
    smell_kind: n_plus_one
    tier: paradigm
    paradigm: orm-with-lazy-loading
    cost_dimension: io
    sub_dimension: database
    locus:
      file: src/handlers/order_handler.py
      lines: "47-89"
      hot_path: request_handler         # iteration-scope tag
    evidence:
      sources:
        - kind: static
          weight: 0.4
          detail:
            ast_pattern: lazy_relation_access_in_loop
            graph_path:
              - OrderHandler.create_order
              - Order.line_items (lazy)
              - LineItem.product (lazy in loop)
        - kind: query_log
          weight: 0.4
          detail:
            log_excerpt: "47 SELECTs against products in 1 request"
            attribution_confidence: 0.92
            sample_request_id: req-abc123
        - kind: profile
          weight: 0.2
          detail:
            sample_count: 1247
            wall_time_share: 0.34
            flamegraph_ref: design/refactor/perf/profiles/order-create.svg
      corroboration_score: 0.91         # composite, see Risks
    measurements:
      blast_radius_direct_callers: 4
      blast_radius_packages_crossed: 1
      local_norm: null                  # n/a for perf; cost_dimension carries the rank
    references_baseline: [BM-001, BM-002]
    blocked_by: []
    corroborates: []
    proposed_remediation:
      transform_kind: eager_load_relation
      sketch: "Replace lazy product access with prefetch_related('line_items__product') on the order queryset."
      atom_available: true
    acceptance:
      detector: n_plus_one
      scope: src/handlers/order_handler.py:create_order
      predicate:
        type: metric_delta
        metric: queries_per_request
        baseline: 47
        target: 1
        minimum_improvement_pct: 95
      secondary_predicates:
        - type: metric_delta
          metric: p99_ms
          baseline: 840
          target_max: 500
          minimum_improvement_pct: 30
      measurement:
        method: query_log_replay
        artifact_ref: design/refactor/perf/baselines/<slug>.yaml
      verification_required: true     # vs. advisory; see Risks

deferred:
  - id: FND-c9d503f1
    cut_reason: "corroboration_score below threshold; only static evidence"
```

### Multi-source corroboration

Each evidence source carries a `weight` (per-source baseline confidence,
adjustable per detector packet). The `corroboration_score` is the composite:

```
corroboration_score = 1 - ∏(1 - weight_i × source_quality_i) for all sources i
```

where `source_quality_i` is the per-source confidence from the detector. The
formula treats sources as independent — N agreeing sources are stronger than
one source repeated N times. The ensemble's cut threshold operates on
corroboration_score; conservative defaults (≥0.80 to enter plan, deferred
below) keep noise out at v0.

### Delta-based acceptance

Acceptance predicates are deltas off baseline measurements, not boolean
re-detection. Five supported predicate types in v0:

```yaml
- type: metric_delta            # decrease (improvement) by absolute or pct
- type: metric_ratio            # bounded ratio (cache_hit_rate ≥ 0.85)
- type: complexity_class_delta  # O(n²) → O(n), structural verification only
- type: query_count_delta       # specialized metric_delta for db query smells
- type: allocation_count_delta  # specialized metric_delta for memory smells
```

PD's recompose evaluates the predicate by re-running the measurement method
declared in `measurement.method` against the post-remediation commit and
comparing to the baseline. When `verification_required: false`, the finding
is advisory and PD marks it `satisfied` on transform completion without
running the verifier.

## Smell taxonomy: universal, paradigm-bounded, language-idiosyncratic

The three-tier taxonomy is inherited from [[Architecture Pathology]]. The
cost-dimension axis is orthogonal — a smell is tier-tagged AND
dimension-tagged. The cut threshold composes both.

### Cost dimensions

| Dimension | What it covers | Detection sources |
|---|---|---|
| `time` | CPU cycles, wall clock, hot-path attribution | static + profile |
| `memory` | Allocations, heap growth, retention | static + profile |
| `io` | Disk and database round-trips, file handles | static + query_log |
| `network` | API calls, RPC fan-out, payload size | static + metric |
| `power` | (deferred to mobile/embedded packs) | profile |

Detectors are tagged with a primary `cost_dimension` and optional sub-
dimension. A single smell can fire on multiple dimensions (e.g. `n_plus_one`
on `io.database` and `time` if the queries are slow); the packet declares
the primary.

### v0 Tier-1 catalog

| Smell kind | Tier | Cost dim | Notes |
|---|---|---|---|
| `nested_iteration_over_same_collection` | Universal | time | O(n²) where hashmap-backed O(n) suffices. Pure AST/dataflow detection |
| `loop_invariant_recomputation` | Universal | time | Pure computation inside loop, hoistable. Dataflow analysis |
| `cartesian_explosion` | Universal | time, memory | Nested iteration emitting all pairs without filtering predicate |
| `redundant_serialization` | Universal | time, memory | Parse/stringify roundtrip on same data within a scope |
| `unbounded_growth` | Universal | memory | Collection or cache without size bound or eviction. Lifecycle analysis |
| `n_plus_one` | Paradigm-bounded (orm-with-lazy-loading OR rpc-in-loop) | io, network | Repeated single-fetch in loop where batched form exists |
| `sequential_await_batchable` | Paradigm-bounded (async-await) | time, network | Sequential `await`s where `Promise.all` / `asyncio.gather` / etc. would parallelize |
| `sync_io_in_hot_path` | Paradigm-bounded (web-request-response OR async-await) | io | Blocking IO inside request handler or async function |
| `missing_covering_index` | Paradigm-bounded (sql-relational) | io | Query reads many columns where existing index could cover. Needs query plan or schema introspection |
| `eager_materialization` | Paradigm-bounded (lazy-eval-streams) | memory, time | `collect()` / `toList()` before filter / take; intermediate collection materialized |

v0 ships the Java language pack and partial coverage of the ORM-with-lazy-
loading and async-await paradigm packs. Other paradigm packs land in
follow-up designs.

### Per-language pack structure

Each language pack implements the universal contracts plus applicable
paradigm contracts plus a language-idiosyncratic catalog. None of the
idiosyncratic detectors below ship at v0.

- **Java**: string concat in loop (use StringBuilder), autoboxing in hot
  paths, `Stream.collect` where `reduce` suffices, `HashMap` vs `TreeMap`
  for sort-required access, `ArrayList.contains` in loop where `Set` would,
  `synchronized` over a `Lock` for fine-grained access.
- **C#**: LINQ-to-objects in hot loop, `IEnumerable` enumerated multiple
  times, missing `ConfigureAwait(false)` in library code, struct-vs-class
  allocation churn, `string` concat in loop, `Span<T>` opportunities.
- **Python**: list comprehension where generator suffices, repeated `.keys()`
  iteration, missing `__slots__` on hot dataclasses, dict-in-dict where
  `defaultdict` is clearer, repeated regex compilation.
- **Elixir**: list prepend in tail-recursive accumulator (correct) vs append
  (O(n²)), `Enum` chain where `Stream` would defer, atom-heavy hot paths,
  binary concat in loops, GenServer call where cast suffices.
- **Rust**: unnecessary `.clone()`, `String` where `&str` works,
  allocations in hot loops, `Arc<Mutex<...>>` where channel-based ownership
  fits, `Vec::contains` in loop, `format!` in hot paths.
- **JavaScript/TypeScript**: huge array chain without breakout, layout
  thrashing in DOM access loops, repeated `JSON.parse`/`stringify`, missing
  memoization in React render, deps-array mistakes, sync `localStorage` in
  hot paths.

### Packet schema

Inherited from [[Architecture Pathology]], with perf-specific fields added:

```yaml
smell_kind: n_plus_one
tier: paradigm
implements_universal: null
paradigm_prerequisites:
  - orm-with-lazy-loading        # ORM packs
  # or
  - rpc-in-loop                  # generic RPC paradigm
cost_dimension: io
sub_dimension: database
language: java                   # null for universal contracts
detection:
  backend: jdtls
  pattern: lazy_relation_access_in_loop
  iteration_scope_check: true    # perf-specific: requires loop-context analysis
evidence_schema:
  sources:                       # perf-specific: multi-source evidence
    - kind: static
      weight: 0.4
    - kind: query_log
      weight: 0.4
    - kind: profile
      weight: 0.2
remediation_template:
  transform_kind: eager_load_relation
  atom: refactor.eager_load
acceptance_template:
  predicate_type: query_count_delta
  default_minimum_improvement_pct: 80
confidence_prior: 0.7
```

## Workflow composition

The `perf-triage` workflow mirrors `arch-triage` (see [[Architecture
Pathology]]) with three differences:

1. **Inlet loads baselines.** Before the detector phase, the inlet resolves
   `<project>/design/refactor/perf/baselines/` and loads the most recent
   baseline-measurements file. If absent and the operator did not supply one
   via `initial_vars.baseline_artifact_path`, the workflow continues with
   `baseline_measurements: []` and tags emitted findings
   `verification_required: false` (advisory mode).
2. **Detector phase composes paradigm packs.** Operators declare paradigms
   in the project refactor hub (e.g. `paradigms: [orm-with-lazy-loading,
   async-await, web-request-response]`). The workflow loads only the
   declared paradigm packs plus the universal contracts plus the language
   pack(s). Same composition principle as language packs, with paradigms as
   an additional declared axis.
3. **Ensemble cut threshold is multi-axis.** Cut score is
   `corroboration_score × cost_dimension_weight / normalized_blast_radius`.
   `cost_dimension_weight` defaults to 1.0 per dimension; operators can
   skew by listing dimensions in `cost_dimensions_in_scope` priority order
   in the plan frontmatter.

Everything else — `corpus-pathfinder` for scouts, the decomposer-panel
pattern, whiteboard-backed corroboration, recompose-council synthesis,
plan-doc + sidecar emission — is shared with `arch-triage`.

## Per-project adoption

Mirrors [[Architecture Pathology]] with three additional steps:

1. **Create the project refactor hub.** Already done if arch-pathology is
   adopted. Add `paradigms:` array alongside `languages:`:

   ```yaml
   languages: [java]
   paradigms: [orm-with-lazy-loading, async-await, web-request-response]
   ```

2. **Set up the perf subdirectory.** `<project>/design/refactor/perf/`
   contains `plans/`, `baselines/`, and `profiles/` subdirectories. Copy
   `examples/perf-pathology/perf-hub.md` to
   `<project>/design/refactor/perf/perf.md` as the perf-specific hub note.

3. **Capture or wire a baseline.** Copy
   `examples/perf-pathology/baseline-suite.template.yaml` to
   `<project>/design/refactor/perf/baselines/<scope>.yaml`. Populate from
   existing benchmarks, slow-query logs, or profile samples. If no baseline
   is available, operators can run the workflow in advisory mode (no
   `verification_required`) and capture baselines as the first plan slice.

4. **Install pathology artifacts.** Run the install block from
   `examples/perf-pathology/install.md`. It installs the universal perf
   contracts, the declared paradigm packs, and the language packs declared
   in `languages:`.

5. **Dispatch the workflow.** `bro_orchestrate_run` with `workflow_id =
   "perf-triage"` and `initial_vars: { project_dir, baseline_artifact_path?,
   cost_dimensions_in_scope?, scope_filter? }`.

6. **Review the plan-doc.** Same convention as arch-pathology — read,
   edit, populate `reviewed_by` and `approved_at`, commit.

7. **Dispatch execution.** Standard `phase-decompose-main-edit`. PD's
   discovery phase recognizes `kind: perf-plan` and routes acceptance
   evaluation to the perf-aware verifier (see [Risks](#risks-and-design-choices)).

8. **Re-measure and re-plan.** Performance plans typically terminate in
   `partial` rather than `satisfied` — once a remediation lands, new
   findings often surface against the moved baseline. The operator runs the
   workflow again against the new baseline.

## Example artifacts shipped

Under `system-defaults/agentic-corpus/`:

```
packets/perf-pathology/
  universal/
    nested-iteration-over-same-collection.json
    loop-invariant-recomputation.json
    cartesian-explosion.json
    redundant-serialization.json
    unbounded-growth.json
  paradigm/
    orm-with-lazy-loading/
      n-plus-one.json
    rpc-in-loop/
      n-plus-one.json                 # same universal contract, different paradigm
    async-await/
      sequential-await-batchable.json
      sync-io-in-hot-path.json
    web-request-response/
      sync-io-in-hot-path.json
    sql-relational/
      missing-covering-index.json
    lazy-eval-streams/
      eager-materialization.json
  languages/
    java/
      nested-iteration-over-same-collection.json
      loop-invariant-recomputation.json
      n-plus-one.json                 # jpa/hibernate backend
      sequential-await-batchable.json # CompletableFuture
      sync-io-in-hot-path.json
      idiosyncratic/                  # empty at v0

brofiles/perf-pathology/
  detector-nested-iteration.json
  detector-loop-invariant.json
  detector-cartesian-explosion.json
  detector-redundant-serialization.json
  detector-unbounded-growth.json
  detector-n-plus-one.json
  detector-await-batchable.json
  detector-sync-io.json
  detector-missing-index.json
  detector-eager-materialization.json
  ensemble-facilitator.json
  ensemble-acceptance.json            # perf-aware variant with delta predicates

teams/perf-pathology/
  perf-detector-panel.json

workflows/perf-pathology/
  perf-triage.json
```

Under `examples/perf-pathology/`:

```
perf-hub.md                           # project-side perf/perf.md template
baseline-suite.template.yaml          # baseline-measurements file template
plan-doc.template.md                  # perf-plan structure illustration
findings.template.yaml                # sidecar with perf extensions
install.md                            # install command block, paradigm-aware
profile-import.md                     # operator playbook for feeding profile samples into evidence
query-log-attribution.md              # operator playbook for query-log attribution
```

The two operator-facing playbooks (`profile-import.md`,
`query-log-attribution.md`) are perf-specific. Architecture pathology has no
equivalent because architecture evidence is purely static.

## Risks and design choices

**Verification cost.** Delta acceptance predicates require re-running the
measurement after each remediation slice. Benchmark suites are slow, query
log replay needs representative traffic, profile samples need reproduction
conditions. PD's recompose either pays this cost per slice (slow) or batches
verification at the end of the plan (loses per-slice attribution on failure).
v0 default: per-slice verification for plans with `verification_required:
true`, deferred batch verification for advisory plans. Operators can
override via `initial_vars.verification_strategy`.

**Multi-source corroboration weighting.** The independence assumption in the
corroboration formula is a simplification — static analysis and profile
samples on the same code aren't truly independent. The weights are operator-
tunable per detector packet, but the math will overstate confidence when
sources correlate strongly. Mitigation for v0: conservative default weights
(static 0.4, query_log 0.4, profile 0.2, metric 0.4) summing well below 1.0
so the composite stays bounded. Calibration is future work; expect the
weights to need adjustment after the first real-codebase run.

**Baseline drift.** Performance baselines reflect a specific commit AND a
specific workload. Workload shifts (traffic patterns, data sizes) can move
baselines without code changes, causing acceptance predicates to fire spurious
failures or false-successes. Mitigation: every baseline file declares
`captured_against_commit` and `captured_at`; PD's recompose flags acceptance
evaluations when HEAD has moved beyond the baseline commit by more than a
configurable threshold of changes to the measured scope. Workload-shift
detection is harder and deferred.

**Detection without runtime context.** Static-only detection of performance
smells has intrinsically higher false-positive rates than architectural
detection. A nested loop over a five-element array is fine; the same pattern
over an unbounded collection is a bug. Iteration-scope analysis
(`hot_path: request_handler | per_row | startup_only`) helps but does not
eliminate false positives. Ensemble cut threshold compensates by requiring
multi-source corroboration; static-only findings default to `deferred` unless
the iteration scope is unambiguously hot.

**Ratcheting plan lifecycle.** Most performance plans terminate in `partial`,
not `satisfied`. Operators new to the workflow expect `satisfied → archived`
parity with architecture plans and read `partial` as failure. Mitigation:
the project perf-hub template explicitly documents the ratcheting lifecycle
as normal. Plans get a `next_baseline_capture_due:` frontmatter hint when
left `partial`.

**Profile and query-log adapter sprawl.** Every monitoring/profiling tool has
its own export format. v0 ships evidence schemas and operator playbooks for
manual pasting; automated ingest adapters would multiply quickly across
tools and languages. The deliberate v0 stance is "evidence in, finding out"
with no built-in ingest pipeline. Adapters are per-tool follow-up designs.

**Cost-dimension misclassification.** A detector tagged `time` may also
heavily impact `memory` for a given workload. The packet declares a primary
dimension, but the ensemble's cut weighting only sees the primary tag.
Operators wanting memory-focused triage on a `time`-tagged smell will need to
manually re-rank. A multi-dimensional weighting model is future work — at v0
the simpler scheme wins on tractability.

## Notes on rejected alternatives

Automated baseline capture as a workflow phase was considered and deferred.
The interaction between baseline capture and CI-vs-local execution
environments, plus the cost of running a benchmark suite as part of every
diagnosis run, makes it too coupled to project-specific infrastructure for
v0. Operators wire in their own capture; the workflow consumes the artifact.

Folding perf-pathology into [[Architecture Pathology]] as a `cost_dimension:`
extension was considered and rejected. The shared substrate is real (workflow
shape, plan-doc shape, taxonomy), but the artifact extensions (baselines,
delta predicates, multi-source evidence) make the contracts genuinely
different, and the lifecycles differ structurally (one-shot vs ratcheting).
Sibling designs keep each lens coherent without forcing premature abstraction
across both.

Extracting a shared "diagnosis pathology" framework with arch-pathology and
perf-pathology as instances was considered and deferred. With only two
instances the abstraction shape is under-determined — better to ship both as
siblings, observe what is actually shared after both exist in artifact form,
and lift the framework when a third lens (security pathology, accessibility
pathology, test-quality pathology) emerges and provides triangulation.

A built-in profile-parsing layer (e.g. flamegraph format ingestion, perf
output parsing) was considered and rejected for v0. Profile tooling sprawl
across languages is too large to handle generically; the per-tool playbooks
keep v0 small while leaving the door open for per-tool follow-up adapters.

## Future work

- **Automated baseline capture phase.** A new workflow node that runs a
  declared benchmark/profile suite before the detector phase and emits the
  baseline artifact directly.
- **Profile and query-log ingest adapters.** Per-tool packets that consume
  raw profile/log output and emit normalized evidence (flamegraph SVG → call
  attribution; pg_stat_statements → query-frequency attribution; Datadog
  trace export → request-path attribution).
- **Workload-aware detection.** Detectors conditioned on actual workload
  shape (read-heavy vs write-heavy, batch vs streaming, peak vs steady).
  Requires runtime metric ingest.
- **Multi-dimensional cost weighting.** Detectors carry weights per
  dimension rather than a single primary; ensemble cut threshold composes
  across dimensions.
- **Regression gating mode.** Read-only perf-triage run on PRs that reports
  finding delta vs main. Same conservative posture as the architecture
  equivalent — advisory only, never blocks.
- **Additional paradigm packs**: GPU compute (CUDA/Metal kernel inefficiencies),
  embedded/mobile power profiles, distributed-systems patterns (chatty
  microservices, missing circuit breakers).
- **Additional language packs**: C#, Elixir, Rust, Python, JavaScript/TypeScript.
  Each implements the universal contracts plus applicable paradigm contracts
  plus a language-idiosyncratic catalog.
- **Diagnosis framework extraction.** Once a third pathology lens lands
  (security, accessibility, observability, test-quality), lift the shared
  substrate (workflow shape, plan-doc artifact, taxonomy framing) into a
  base design that all three instantiate.

## Relationship to existing designs

- Sibling to [Architecture Pathology](arch-pathology.md). Shared substrate;
  divergent artifact extensions and lifecycle. Operators typically adopt
  one first and the other after the workflow shape is familiar.
- Built on top of [Phase-Decompose
  Dispatch](../orchestration/phase-decompose/) — consumes the existing
  `main-edit` lane unchanged, with one perf-specific extension on PD's
  discovery phase to route delta acceptance predicates to a perf-aware
  verifier.
- Pairs with the per-language [Refactor Tools](refactor-tools.md) clusters
  under `refactor-tools/<language>/`. Performance language packs ship
  alongside architecture language packs; the universal and paradigm
  contracts are shared at the pathology level but the detection backends
  live under each language's directory.
- Per-project artifacts live under `<project>/design/refactor/perf/` and
  follow the [Design Corpus](../design-corpus.md) frontmatter conventions.
- A pathology-derived plan typically runs as a [Refactor Compound
  Run](refactor-compound-runs.md); performance plans more frequently
  ratchet — each compound run lands a slice, the next plan captures the
  moved baseline.

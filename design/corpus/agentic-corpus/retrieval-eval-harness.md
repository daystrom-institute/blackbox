---
title: "Retrieval Eval Harness — Mode Decomposition + Stage-Funnel Diagnostics"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
---

# Retrieval Eval Harness

Date: 2026-06-02
Status: proposed — net-new; Brick 3 of the locate-information coherence path.

Related:
- [Locate-Information Coherence Path](locate-information-coherence.md) — parent arc; this is its Brick 3.
- `src/mcp_tools/hybrid_search.rs` — `hybrid_search_typed`; the retrieval pipeline this harness grades.
- `src/search/rrf.rs` — `fuse_rrf`. `src/search/rerank.rs` — `apply_rerank`.
- `src/index/search.rs` — BM25 fetch (`hybrid_bm25_hits`); `bbox_search`.
- `src/mcp_tools/inspect.rs`, `find_paths.rs`, `bundle_evidence.rs` — the traversal/bundling tools graded by Conditioned/EndToEnd modes.
- `src/server/state.rs` — `SharedState::for_test`; isolated index construction for fixtures.
- Spike provenance: `../daystrom-mk2/spikes/Daystrom.Spike.McpPoc/EvaluationHarness.cs`.

## Why

Blackbox tunes retrieval blind. There is no instrument that answers "for a
question whose answer we *know*, where in the pipeline did the answer get lost?"
Without it, the coherence path's Brick 2 (indexing system memories/packets,
demoting `bbox_knowledge` to a lens) would be asserted, not measured — and the
RRF/rerank/dedup/diversify passes already in `hybrid_search_typed` are tuned by
intuition. This harness is what let the Daystrom spike iterate on tiering and
descriptions with evidence (it reports recall lifts like 23% → 97% from the
dedup-by-file + diversify-by-type passes that blackbox later ported).

The harness has one job: take a suite of `question → expected answer entities`,
run blackbox's real retrieval over a controlled corpus, and for every expected
entity that did not surface, classify **which stage dropped it**.

## What to port vs re-derive

`EvaluationHarness.cs` contributes the *discipline*, not the code:

- **Port the shape:** three eval modes; a precedence-ordered miss funnel;
  per-query and aggregate scoreboards; structured JSON out + human summary;
  benchmark + held-out suites.
- **Re-derive the funnel:** Daystrom's `MissStage`
  (`Unreachable > NotMaterialized > NotSelected > RankedTooLow`) keys off *its*
  graph materialization stages (`reachable`, `inMaterializedGraph`,
  `inSelectedGraph`, `finalRank`). Blackbox's retrieval pipeline has different
  stages (BM25/vector fetch → RRF fusion → rerank → filters → dedup/diversify →
  truncate), so the funnel must be rebuilt against *those* stages.

## The retrieval pipeline (the thing being graded)

`hybrid_search_typed` (`src/mcp_tools/hybrid_search.rs`) runs, in order:

1. **BM25 fetch** — `hybrid_bm25_hits(query, bm25_fetch = limit*32, doc_type)`.
2. **File-level BM25 aggregation** — `aggregate_bm25_by_file` adds a second
   ranked list (coverage-weighted `sum * sqrt(count)`).
3. **Vector fetch** — `vector_ranked_lists(...)` per embedding route/partition.
4. **RRF fusion** — `rrf::fuse_rrf(lists, RRF_K, fetch)` over BM25 + aggregated
   + vector lists.
5. **Feature enrichment** — `enrich_fused_features` loads entity properties.
6. **Rerank** — `rerank::apply_rerank(score, feature, now)` (type/recency
   multipliers), then sort by reranked score.
7. **Project-scope filter** — `retain(keep_under_project_filter)` when `project`
   set.
8. **doc_type filter** — `retain(doc_type == p.doc_type)` when set.
9. **File dedup** — `retain` best chunk per `file_dedup_key`.
10. **Modal diversification** — `diversify_by_chunk_kind(results, limit)`.
11. **Truncate to `limit`.**

`bbox_search` (transcript-only) is a simpler subset (BM25 + snippet, no vector
fusion); the harness grades it with the same funnel minus the vector/fusion
stages.

## The stage funnel (blackbox MissStage)

For each expected entity not in the returned top-N, assign the **furthest stage
it reached**, in precedence order (highest = earliest loss):

| Verdict | Reached as far as | Fix it points to |
|---|---|---|
| `NotIndexed` | no tantivy doc for the entity at all | **Brick 2** — the store isn't an indexed doc type (system memories, packets today) |
| `NotRetrieved` | indexed, but absent from both BM25 (even at depth `limit*32`) and vector fetch | lexical vs semantic gap — tokenizer, chunking, or embedding route; sub-tag `bm25_miss` / `vector_miss` |
| `FusedTooLow` | present in a ranked list, but RRF ranked it below `fetch` | `RRF_K`, list weighting, the file-aggregation blend |
| `RerankedDown` | survived fusion, but `apply_rerank` pushed it below kept entries | type/recency multipliers over-penalizing the entity's kind |
| `DroppedByFilter` | survived rerank, removed by project / doc_type / file-dedup / diversify | filter false-positive — the blackbox-specific stage with no Daystrom analogue |
| `RankedTooLow` | in the final set, below the returned `limit` / the assertion's top-N | `limit`, or genuine ranking weakness — broadest "tune the scorer" bucket |
| `Passed` | in top-N | — |

The point is the same as Daystrom's: "search feels bad" is useless; "8 expected
runbooks were `NotIndexed`, 3 entries `FusedTooLow`, 1 `DroppedByFilter`" names
three different, separately-actionable fixes — and the first bucket *is* the
Brick 2 thesis, quantified.

### The trace hook (net-new instrumentation)

`hybrid_search_typed` returns only the final `HybridResult` list; the
intermediate lists (BM25 hits, fused order, pre-truncate results) are dropped.
To assign a funnel verdict the harness needs the furthest stage a *target*
entity reached. Add an opt-in trace:

- A debug variant — `hybrid_search_traced(p, target: &EntityRef) ->
  (HybridSearchResponse, StageTrace)` — that records, for `target` only, a
  boolean per stage (`in_bm25`, `in_vector`, `in_fused`, `fused_rank`,
  `post_rerank_rank`, `survived_filters`, `final_rank`). Cheap: one membership
  check per stage against a single id; gated behind the eval path, not the live
  tool.
- The harness maps that `StageTrace` to a `MissStage` verdict via the precedence
  table.

This is the blackbox analogue of Daystrom's per-entity `EntityDiagnostic` and is
the only retrieval-code change the harness strictly requires.

## Modes

Three modes isolate *where* the system fails, mirroring `EvaluationHarness.cs`.
The first two are deterministic and need **no LLM**; only EndToEnd does.

- **SearchOnly** — `question → hybrid_search`. Did the expected entity surface,
  and at what rank? Isolates pure retrieval ranking. Emits the stage-funnel
  verdict per expected entity. This is the workhorse and the cheapest signal.
- **Conditioned** — inject the *known-correct* seed ref, skip retrieval, and run
  the downstream tools (`inspect_entity` → `find_paths` → `bundle_evidence`).
  Does the expected *answer* entity surface given the right seed? Isolates
  traversal/bundling from retrieval ranking — a Brick-2 lens regression shows
  here even when SearchOnly is green.
- **EndToEnd** — `question → hybrid_search → pick top seed → inspect/paths →
  bundle`. Full pipeline; the only mode whose seed selection and synthesis need
  an agent turn (bounded LLM spend, see Run surface).

## Bundle A/B (grades Brick 2's consolidation)

Per query, compare two ways of answering and measure cost, not just
correctness — the metric that proves "one path, many filters" beats parallel
matchers:

- **Bundle path:** single `bbox_bundle_evidence` over the funnel's top refs.
- **Baseline path:** the manual sequence (`hybrid_search` + N `inspect_entity`
  calls).
- Record: answer-correctness, grounding (did it produce typed evidence/paths),
  tool-call count, and token estimate. Aggregate into a reduction ratio
  (Daystrom reported ~1 vs 3–4 calls and a token ratio per query).

## Suite format & corpus isolation

**Suite** — checked-in fixtures, one row per query:

```
{ id, question, expected_entity_refs: [...], mode,
  seed_ref?,          // Conditioned mode
  doc_type?, project?, // exercise the filters
  top_n: 10 }          // assertion window
```

Two suites per the overfit guard: a small **benchmark** (authored against known
content) and a **held-out** set not consulted while tuning.

**Corpus isolation (hard requirement).** The harness MUST build a dedicated
index from fixture documents in a tempdir via `SharedState::for_test`, never
query the prod index on `127.0.0.1:7264` or touch real `$HOME`/XDG/the shared
tantivy index. Per the repo's Rust test-isolation invariants: per-test tempdirs,
canonicalize tempdir roots before path asserts, hold `test_env_lock()` for any
process-env mutation. A shared/prod index makes results nondeterministic and
collides with peer agents and worktrees. The fixture corpus is the *controlled
input* the whole funnel-classification depends on — expected refs are only
meaningful against a corpus the harness fully owns.

## Run surface

- **Offline, deterministic core (SearchOnly + Conditioned):** a `cargo test`
  target or a small example/bin (`examples/` or `src/bin/`) that builds the
  fixture index, runs the suite, and emits the scoreboard. No LLM, no network —
  reproducible in CI and by any agent/worktree.
- **EndToEnd:** behind an explicit flag with a bounded LLM-spend budget (reuse
  the Fleet-TUI "limited LLM spend authorized, keep probes narrow" convention).
  Off by default.
- **Output:** structured JSON to stdout (machine-trackable across runs), human
  markdown summary to stderr — exactly the `EvaluationHarness.cs` split.
- **Not** an MCP `bbox_*` tool: this is dev/CI instrumentation, not an
  agent-facing surface. If agents ever need self-eval, expose it then, behind
  the appropriate deferred/`work_*` boundary.

## Metrics & scoreboard

- Per mode: pass rate (expected refs in top-N).
- **Failure-stage histogram** — the headline: count by `MissStage` across the
  suite, so the single highest-leverage fix is obvious.
- `recall@k` at a couple of k values.
- Bundle A/B: correctness/grounding deltas + tool-call and token reductions.
- Cross-run JSON so a tuning change's effect is a diff, not a vibe.

## Build order (net-new code)

1. Fixture schema + a tiny benchmark suite + tempdir corpus builder
   (`SharedState::for_test`).
2. `hybrid_search_traced` trace hook + `StageTrace → MissStage` mapping.
3. SearchOnly runner + failure-stage histogram + JSON/markdown emitter.
4. Conditioned runner (seed injection through inspect/paths/bundle).
5. Bundle A/B comparator.
6. EndToEnd runner (flagged, budgeted) — last, and optional for the Brick 2 gate.

Steps 1–3 are the minimum that makes Brick 2 measurable; 4–6 deepen it.

## Open questions

- **Suite authorship:** hand-author fixtures, or mine real
  `sm-agentic-opening-sequence` / `bbox_cite` traces for question→answer pairs?
- **Held-out discipline:** who/what guarantees the held-out set stays unconsulted
  during tuning.
- **Trace generality:** trace a single target per call (simple) vs all expected
  refs for a query in one pass (fewer re-runs, more bookkeeping).
- **`bbox_search` parity:** one funnel with vector/fusion stages marked N/A for
  transcript-only search, or a separate reduced funnel.

## Status

Net-new, nothing landed. Sequence after — or alongside — Brick 2: stand up
steps 1–3 to baseline current retrieval, land Brick 2, re-run, and read the
`NotIndexed` bucket collapse as the proof.

---
title: "Locate-Information Coherence Path — Unifying Retrieval Across Knowledge, Graph, and Memory"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - knowledge
---

# Locate-Information Coherence Path

Date: 2026-06-02
Status: partial — Bricks 0–1 landed on `feat/knowledge-locate-coherence`; Bricks 2–3 proposed.

Related:
- `src/tools/knowledge.rs` — `bbox_knowledge` adapter; fuses three stores (entries + packets + memories).
- `src/knowledge.rs` — `Knowledge::list`; per-entry excerpt (`KNOWLEDGE_EXCERPT_BYTES`), `limit`.
- `src/system_memory/catalog.rs` — `format_for_signpost`, `format_for_listing`, in-memory `search`.
- `src/mcp_tools/hybrid_search.rs` — `HybridSearchResponse.next_steps`, `build_next_steps`; pre-existing Daystrom-derived dedup/diversify passes.
- `src/mcp_tools/inspect.rs` — `recommended_next_hops`. `src/mcp_tools/discover_seed.rs` — `notable_edges`.
- `src/index/search.rs` — `bbox_search` render + breadcrumb footer.
- `src/entity_ref.rs` — `EntityType` taxonomy (graph entities); no `SystemMemory` variant today.
- `src/embed/mod.rs` — `Bucket` enum (embedding routes).
- Gap: `af74086b` (mcp_surface / knowledge / broad-query-output-bounding).
- Spike provenance: `../daystrom-mk2/spikes/Daystrom.Spike.McpPoc/AgenticTools.cs`, `EvaluationHarness.cs`.

## Problem

The proximate trigger was gap `af74086b`: a broad smart-mode `bbox_knowledge`
query returned ~81k chars (twice in one session), overflowing the token budget
and forcing a harness spill-to-file. The root cause was narrow — system
memories were the lone unbounded surface (full ~40KB runbook bodies dumped for
every fuzzy match) while knowledge entries (120-byte excerpts) and rule-packets
(one-line rows) were already bounded.

But the gap exposed a deeper structural issue. Blackbox has **three
disconnected retrieval planes**, and an agent's mental model of "how do I locate
information here?" has no single answer. `bbox_knowledge` in particular is not a
discovery tool — it is a fuzzy string-matcher that requires *a priori* knowledge
of what is stored. The working idiom is "I already know `sm-refactor-rust`
exists, so I query for it and follow its signposts" — that is retrieval by
known id, not discovery by question.

The opposite is the goal: **a question-shaped query should surface the runbook,
knowledge entry, or packet that answers it**, without the agent knowing the
artifact exists. That is precisely what proper indexing + evidence bundling
buys, and it is the through-line of this design.

## What the Daystrom spike proved

The `daystrom-mk2` MCP PoC (`AgenticTools.cs`, `EvaluationHarness.cs`) is a
purpose-built agentic graph-navigation harness with a measured eval loop. It
isolates three levers plus a measurement discipline.

**1. Tool shape — a typed, linear, self-narrowing funnel.**
`discover_seed_entities → inspect_entity → find_paths → bundle_evidence`, with
`list_edge_types` as a vocabulary primer. Two shape decisions carry the weight:
- **Typed refs (`Type:Id`) are the universal currency.** Every tool emits them,
  every tool consumes them, and every description repeats it ("Returns Type:Id
  format for direct use in other tools"). No restating from memory.
- **Tiered verbosity, bounded by default.** `property_mode = summary | smart |
  full`; `smart` (default) truncates fields >300 chars, edges capped at
  `per_type_limit=5` with an explicit `... +N more`. The cheap view is the
  default; `full` is opt-in.

**2. Response breadcrumbs — the primary unlock.** Each tool output ends by
telling the agent the next tool and the exact tokens to paste: `discover` →
`Type:Id`; `inspect` → "Recommended next hops" (type-aware, targeting the
"one hop short" problem); `find_paths` → "use path IDs P1,P3 in
bundle_evidence" with globally-stable IDs that accumulate across calls;
`bundle_evidence` → re-validates that cited edges exist. A response breadcrumb
beats a prompt instruction because it is injected at the decision point, not
recalled from a memory read 40 turns ago.

**3. Opinionated descriptions as a tuning surface.** The spike's own changelog
lists "Stronger tool descriptions pushing targeted edge_types and directional
traversal" as a measured fix — prose treated as a tunable parameter.

**4. The eval harness made all of it knowable.** `EvaluationHarness.cs`:
- **Mode decomposition** — `SearchOnly` (does the seed rank?) / `Conditioned`
  (given the right seed, does traversal find the answer?) / `EndToEnd`.
  Separates retrieval failure from traversal failure.
- **`MissStage` funnel** — for every expected entity that did not surface,
  classify *why* in precedence order: `Unreachable > NotMaterialized >
  NotSelected > RankedTooLow > Passed`. Aggregated across a suite, this turns
  "search feels bad" into "12 answers materialized but ranked >10, 3
  unreachable" — different fixes entirely.
- Bundle A/B comparison quantifies tool-calls and tokens saved by the
  consolidated path; benchmark + held-out suites guard against overfitting.

## Diagnosis — three retrieval planes

| Plane | Members | Reached via | Daystrom levers applied? |
|---|---|---|---|
| **Indexed corpus** (tantivy BM25 + vector + graph) | `knowledge`, `transcript`, `project_file`, `thread`, `commit`, `note`, `roadmap` | `hybrid_search`, `discover_seed_entities`, `bbox_search` | yes — typed refs, breadcrumbs, tiering |
| **In-memory rule stores** | rule-packets, system memories | **only** `bbox_knowledge` (string-match, was full-body dump) | no — not indexed, not graph-addressable |
| **Artifact/agent catalogs** | agents, atoms, workflows, brofiles, artifacts | bespoke `*_list` / `*_search` / `*_describe` | no — each its own shape |

The decisive evidence: `EntityType` (`src/entity_ref.rs`) *includes* `Packet`
and `Agent` as graph entities, but the search index (`add_text(f.doc_type, …)`
across `src/index/`) only covers `project_file, thread, commit, knowledge,
roadmap` (+`transcript`/`note`). So packets/agents are graph nodes you can
*traverse to* but cannot *retrieve by content* in `hybrid_search`. System
memories are not even graph entities — they are a parallel file catalog reached
only through `bbox_knowledge`'s string matcher. That is why rule-packets "feel
out of place" surfacing under `bbox_knowledge`: it is the only tool that bolts
the in-memory stores onto a query, and it did so without index, graph, or
tiering.

The graph plane is itself a faithful Daystrom port: `bbox_inspect_entity`'s
description mirrors `property_mode summary/smart/full`, and
`recommended_next_hops` (`inspect.rs`) / `notable_edges` (`discover_seed.rs`)
are direct analogues of `BuildNotableEdges`. The lessons were already applied to
*one* plane. The coherence path applies them across *all* of them.

## The coherence path (brick sequence)

### Brick 0 — tiered verbosity for the in-memory store *(landed)*

System memories now render as compact **signposts** on the broad
`bbox_knowledge` path (`format_for_signpost`): header + tags + one-line preview
+ a `bbox_knowledge(query="sm-…")` retrieval breadcrumb. The exact-id
short-circuit (`exact_system_memory_response`) still returns the full body, so
"pull the doc by qualified name" is unchanged. This is Daystrom lever #1
(summary default, full opt-in) applied to plane 2, and it closes gap
`af74086b`. Empty-query worst case (all 28 memories) ≈ 13KB vs the ~81KB
overflow.

### Brick 1 — response breadcrumbs across every entry point *(landed)*

Every locate-information surface now ends by naming the next tool with concrete
refs:
- `bbox_hybrid_search` → structured `next_steps` + text footer carrying the top
  seed ref into `inspect_entity` / `find_paths` / `bundle_evidence`; empty
  results yield a broaden-the-query hint.
- `bbox_search` → footer with the top hit's coordinates for
  `bbox_context(file, offset)` / `bbox_messages(session)` / `bbox_cite`.
- `bbox_knowledge` → top-level "Next steps" pulling the highest-ranked entry
  into `inspect_entity` + `bundle_evidence`; packets already carried
  `bbox_apply`, memories carry the Brick-0 signpost.

The discover → inspect → paths → bundle sequence is now injected at each
decision point rather than recalled from `sm-agentic-opening-sequence`. This is
the highest ROI-per-line change and the part the graph plane was missing at its
entry points.

### Brick 2 — index unification *(proposed; the architectural fork)*

Make the in-memory rule stores first-class so a **question-shaped query**
surfaces them through the same funnel as everything else, and demote
`bbox_knowledge` from a parallel string-matcher to a **lens** over the index.

Concretely:
- New indexed doc types for **system memories** and **rule-packets**
  (docbuilders alongside `src/index/knowledge_docs.rs` /
  `thread_docs.rs`), wired into the reindex pipeline and an embedding
  `Bucket` (reuse `Knowledge` or add dedicated routes; see `src/embed/mod.rs`).
- New `EntityType::SystemMemory` in `src/entity_ref.rs` (ref grammar
  `system_memory:<id>`) so memories become **inspectable and bundleable** — a
  runbook can then be an answer entity in an evidence bundle, with edges to the
  atoms/tools it signposts.
- `bbox_knowledge` becomes `hybrid_search` filtered to
  `knowledge | packet | system_memory` with the tiered renderer (signpost
  default, full on exact id). One retrieval path, three filters — not three
  retrieval paths.

**The fork:** *index* the in-memory stores (this brick) vs. keep them in-memory
and only give `bbox_knowledge` the tiered renderer + breadcrumbs (cheaper,
preserves the bounded-but-still-a-priori path). The decision is settled by the
goal in the Problem section: only real indexing delivers question-shaped → memory
surfaced. Brick 0's tiered renderer is the cheap fallback if Brick 2 is
deferred; it is not a substitute for it.

**Open questions for Brick 2:**
- Embedding bucket: reuse `Knowledge` (simplest) vs a `Memories`/`Packets`
  route (cleaner partition metrics, more config surface).
- System-memory chunking: whole-body single doc vs per-section chunks (the big
  runbooks are ~40KB; per-section lifts recall, matching the project-file
  aggregation rationale in `hybrid_search.rs`).
- Graph edges for memories: do we materialize `SystemMemory --SIGNPOSTS-->
  Atom/Tool` edges from the runbook prose, or leave memories edge-light?
- Lens migration: keep the `category="system_memory"` catalog listing and the
  exact-id short-circuit as fast paths, or route everything through the index.

### Brick 3 — the eval harness *(proposed; do before/with Brick 2)*

Port the Daystrom measurement discipline so Brick 2 is measured, not asserted.
Minimum viable:
- A small query suite (question → expected answer entity/runbook).
- **Mode decomposition**: does the seed rank (retrieval) vs does the known
  answer surface given the seed (traversal/bundling).
- **`MissStage`-style classification** of every miss, so we know whether a
  question-shaped memory query fails because the memory is *not indexed*, *not
  ranked*, or *not selected*.
- A bounded LLM-spend budget; structured JSON out, human summary to stderr.

This is what let Daystrom iterate on tiering and descriptions with evidence. It
should land before or alongside Brick 2 so the structural change is graded.

## Design principles (distilled)

1. **Typed refs are the universal currency** — every surface emits canonical
   `<type>:<segments>` refs; every description says so. (Already enforced by
   `entity_ref.rs`; extend to system memories in Brick 2.)
2. **Tiered verbosity, bounded by default** — summary/signpost is the default;
   full body is opt-in or by exact id. (Brick 0; generalize in Brick 2.)
3. **Response breadcrumbs > prompt instructions** — inject the next step at the
   decision point, carrying the exact tokens. (Brick 1.)
4. **One retrieval path, many filters** — discovery is a single funnel; stores
   are filters over it, not parallel matchers. (Brick 2.)
5. **Eval-driven** — instrument *where* retrieval fails (MissStage), not just
   *that* it failed. (Brick 3.)

## Status & pickup contract

- **Landed** (`feat/knowledge-locate-coherence`): Brick 0 (`f084a65`), Brick 1
  hybrid_search (`99de123`), Brick 1 search+knowledge (`2ac3c58`). Gap
  `af74086b` is fixed by Brick 0 but not yet resolved/merged.
- **Next**: Brick 3 (eval harness) to establish a baseline, then Brick 2 (index
  unification) graded against it. Both are fresh-session-sized.
- **Do not** treat Brick 0's signpost renderer as the end state — it bounds the
  output but leaves `bbox_knowledge` a-priori. The goal is question-shaped
  surfacing via the index.

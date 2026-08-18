---
title: "Gap-to-Campaign Migration Plan: Open Gaps as Graph Inquiries under Campaigns"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - graph
  - design-corpus
tags: [design-corpus, graph, gaps, campaigns, migration-plan]
brief: "Plan only: map 19 open gap-log records onto five proposed dsg:Campaign vertices with a per-gap dsg:Inquiry or dsg:Concept stub, each SOURCED_FROM its GapRef, using the schema v2 campaign layer. No vertices are created and no gap is edited by this document."
date: 2026-08-18
---

# Gap-to-Campaign Migration Plan

Status: proposed (plan only; execution is operator-gated)

Related:

- [Design Graph](design-graph.md) - the graph this plan extends; state/story
  split, verb-script authorship, commitment gate.
- `docs/design-graph.md` - the hands-on authoring loop.
- `.bbox/graphs/design/schema.json` (v2) - the campaign layer this plan
  targets: `dsg:Campaign`, `dsg:Inquiry`, `dsg:GapRef`, `dsg:PART_OF`,
  `dsg:SOURCED_FROM`.
- `.bbox/gaps/` - the gap log; every gap named below is read there, never
  edited here.

## Why

The gap log holds durable substrate gaps as flat records: each one is a
capability someone wanted and did not have, filed at the moment of friction.
That is the right shape for filing and closing, and the wrong shape for
planning: nineteen open gaps today form five recognizable initiatives, but the
grouping lives in nobody's head and in no queryable store. The gap log cannot
say "these six gaps are one campaign, this is its status, and these two designs
anchor it", and the design graph until now could not point back at a gap.

Schema v2 of the design graph adds a campaign layer for exactly this:

- `dsg:Campaign` (`campaign/<slug>`): a durable initiative grouping related
  work; `slug`, indexed `summary`, `status` in `proposed | active | parked |
  done`, optional `outcome`.
- `dsg:Inquiry` (`inquiry/<slug>`): a question or concept pulled from the gap
  log that needs investigation before it becomes design or work; indexed
  `summary`, `status` in `open | concluded`, optional `kind` in `question |
  concept`, indexed `outcome` (lint requires an outcome once concluded).
- `dsg:GapRef` (`gap/<gap-id>`): a mirror vertex for one gap-log record
  (`gap_id`, optional `dedupe_key`, indexed `title`). It exists because
  substrate v1 edges connect graph vertices only; the mirror keeps
  `SOURCED_FROM` traversable in both directions and gives a later gap closure
  a vertex to cite. `lint` refuses a GapRef whose `gap_id` is malformed,
  disagrees with its id, or names no record under `.bbox/gaps/`.
- `dsg:PART_OF` (Concept, Inquiry, OpenQuestion, Decision -> Campaign) and
  `dsg:SOURCED_FROM` (any vertex -> GapRef, optional `note`).
- `dsg:RELATES_TO` gains `Campaign -> Design` (anchoring designs) and
  `Inquiry -> Concept | Design` endpoints; `dsg:DEPENDS_ON` gains
  `Campaign -> Campaign` so `blockers` and `frontier` can read prerequisites.

A `dsg:Finding` kind was considered and left out. Schema-wise it is cheap (one
more vertex block), but nothing would author it yet: an Inquiry's `outcome`
already carries "what we found", and the gap log stays the filing channel for
substrate drift per the design-graph boundary rules. Adding a kind with no
edge family and no writer is speculative surface; mint it when a second
consumer shows up.

## Boundary held

- The gap log stays authoritative for the gap itself: title, kind, domain,
  wanted capability, evidence, resolution. The graph never copies gap status
  onto a vertex (implementation state is computed or anchored, never stored).
  A GapRef carries identity plus a display title, nothing that can drift.
- Closing a gap later cites the vertex: `bbox_gap_resolve` notes reference
  `project_graph_vertex:<project>:design:inquiry/<slug>` (or the campaign),
  and the graph side flips the Inquiry to `concluded` with an `outcome` in the
  same operator pass. Neither store absorbs the other.
- Everything below is FILE-level state under the commitment gate: campaigns
  land at `status: proposed`, inquiries at `status: open`. Only an
  operator-ratified pass flips a campaign to `active`, `parked`, or `done`.

## The mapping

Five proposed campaigns; nineteen gaps; one Inquiry or Concept stub per gap.
"Inquiry" is used where the gap still holds an open question (what, how, or
whether); "Concept" is used where the gap names a durable idea that already
has a second articulation (a design doc plus the gap) and only needs a home in
the graph. Slugs are proposals; the executing pass may rename before minting.

### Campaign 1: `campaign/corpus-multimodal-depth`

Summary: extend the agentic corpus beyond text-and-code: multimodal chunk
markers, per-language AST depth through the LSP substrate, first-class file
entities, cross-reference edge resolution, and a learned rerank stage. Anchor
designs (`RELATES_TO`): `doc/design/corpus/agentic-corpus/multimodal-embedding-routing.md`,
`doc/design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md`,
`doc/design/corpus/agentic-corpus/agentic-corpus-tier-b-ast.md`.

| Gap | Title | Stub | Slug | Kind | One-line statement |
|---|---|---|---|---|---|
| gap-d5bd0c66 | Multimodal X-* chunker markers (7 formats) | Inquiry | `inquiry/multimodal-x-markers-after-routing-layer-0` | question | Once the Layer 0 routing substrate and the multimodal provider land, which of the seven deferred X-* formats ship first, and does each need its own compatibility family? |
| gap-94916a57 | Y-* AST depth markers (5 remaining languages) | Inquiry | `inquiry/lsp-depth-remaining-languages` | question | Which of the five remaining language servers earn wiring through the LSP session manager, in what order, and what does "shipped" mean per language (markers, tests, failure mode)? |
| gap-3c03bc64 | chunk_kind expansion for IMG/AV | Concept | `concept/chunk-kind-img-av` | - | The chunk_kind enum accepts image and audio/video variants and the chunker emits them; a durable idea already articulated by the chunkers design and the gap. |
| gap-99e7a909 | Markdown link target resolution | Inquiry | `inquiry/markdown-link-target-resolution` | question | Does an EdgeIndex chunk-by-(file, content_hash) lookup resolve markdown file and section links into entity edges, or does the file entity below subsume it? |
| gap-ab3ef97f | file: virtual entity for IN_FILE cleanup | Concept | `concept/file-virtual-entity` | - | A first-class file entity replaces the chunk[0]-as-file proxy for IN_FILE edges (schema bump); articulated by the search-quality walk and the gap. |
| gap-85c45849 | Per-turn LLM scoring for ranker | Inquiry | `inquiry/rerank-stage-vs-per-turn-scoring` | question | Does a cross-encoder rerank stage over the fused top-k beat per-turn LLM scoring on the shipped MRR/recall metrics, and at what k? |

### Campaign 2: `campaign/provenance-completion`

Summary: close the gaps between the provenance design and what the runtime
records: historical backfill of tool-call edges, arc-produced-commit wiring,
git-notes sync automation, and a generic anchor-indexed lookup. Anchor design:
`doc/design/corpus/commit-work-provenance.md`.

| Gap | Title | Stub | Slug | Kind | One-line statement |
|---|---|---|---|---|---|
| gap-ef78d005 | Backfill tool-call edges for newly-registered projects | Inquiry | `inquiry/provenance-backfill-on-reindex` | question | Should reindex backfill tool-call provenance edges from historical transcripts, and what bounds the cost on a large corpus? |
| gap-718d5b26 | ARC_PRODUCED_COMMIT edge wiring | Concept | `concept/arc-produced-commit-edge` | - | An arc records its producing commit at exit and emits an ARC_PRODUCED_COMMIT edge; designed, advertised in schema, not yet wired. |
| gap-f9f68f7c | Bidirectional git-notes sync hooks | Inquiry | `inquiry/git-notes-sync-automation` | question | Git hooks or a daemon subscription: which mechanism auto-exports provenance notes on commit and imports on fetch without surprising operators? |
| gap-311023fd | Anchor-indexed provenance lookups | Inquiry | `inquiry/generic-anchor-index` | concept | Generalize the per-commit and per-session anchor indices into one anchor-indexed lookup that future provenance walks reuse. |

### Campaign 3: `campaign/eval-coverage`

Summary: turn designed evaluation into standing gates: audit example sets in
CI, the remaining probe-team question shapes, live proof of the pathology
ensemble family, and richer per-class checker semantics. Anchor design:
`doc/design/corpus/agentic-corpus/retrieval-eval-harness.md`.

| Gap | Title | Stub | Slug | Kind | One-line statement |
|---|---|---|---|---|---|
| gap-5ec87592 | Audit example sets CI integration | Inquiry | `inquiry/audit-sets-as-ci-gate` | question | Cargo test task or CI step: which runs `bbox_audit` over every `eval/audit/<domain>/*.json` at PR time without a live daemon? |
| gap-9d84f24f | Probe-team Q2-Q8 | Inquiry | `inquiry/probe-team-remaining-shapes` | question | Dispatch the seven remaining probe-team question shapes with their checklists; what does the cold-start grounding baseline look like per shape? |
| gap-9d0f9159 | Pathology ensemble flows unproven | Inquiry | `inquiry/pathology-ensemble-live-proof` | question | Prove the three unproven pathology flows and re-run the heterogeneous panel on real providers; requires the prod daemon host and healthy providers. |
| gap-b45dc2d0 | Per-class checker logic beyond Any/All/First | Inquiry | `inquiry/per-class-checker-semantics` | concept | Per-query-class checker richness (at-least-n, must-include-path-validation, weighted contributions) beyond the v1 pass_strictness. |

### Campaign 4: `campaign/pipeline-wiring`

Summary: finish deferred plumbing in the deterministic orchestration
pipelines: ensemble output boundaries, multi-partition compaction per tick,
and source-query propagation into auto-digest. Anchor: `doc/design/orchestration/`
workflow designs as identified by the executing pass (no single doc anchors
all three; leave `RELATES_TO` empty rather than guess).

| Gap | Title | Stub | Slug | Kind | One-line statement |
|---|---|---|---|---|---|
| gap-7606edc2 | Auto-edge ensemble vote parsing | Inquiry | `inquiry/ensemble-output-boundaries` | question | Per-member output array or explicit member boundaries: which lets `parse_json` disambiguate ensemble votes with the least workflow churn? |
| gap-bfe61876 | Multi-partition compaction per arc tick | Concept | `concept/compaction-all-stale-partitions-per-tick` | - | The compaction tick iterates every stale partition through the foreach primitive instead of worst-only per cron; unblocked, wiring deferred. |
| gap-17d65325 | Auto-digest source_query plumbing | Inquiry | `inquiry/task-completed-carries-source-query` | question | When the task-completed signal triggers auto-digest, how does the trigger payload carry the originating source_query end to end? |

### Campaign 5: `campaign/structural-guardrails`

Summary: structural fixes the substrate keeps working around: a per-turn tool
call budget for agentic actors, and explicit vector-store passing in place of
the module-level singleton. Anchor design:
`doc/design/daemon-runtime/concurrency-model.md` (executing pass confirms the
doc id).

| Gap | Title | Stub | Slug | Kind | One-line statement |
|---|---|---|---|---|---|
| gap-fdacb6ed | Per-turn MCP tool-call budget for agentic actor | Inquiry | `inquiry/per-turn-tool-call-budget` | question | What primitive budgets tool calls per LLM turn (not per workflow node), and does the eval show the runaway-loop failure the soft prompt budget was meant to hold off? |
| gap-a02e5c7d | VectorStore singleton refactor | Concept | `concept/vector-store-explicit-passing` | - | Thread `&VectorStore` through call sites instead of the module-level singleton so tests inject isolated stores. |

Counts: 6 + 4 + 4 + 3 + 2 = 19 gaps; 13 Inquiry stubs, 6 Concept stubs.

Note on the Concept stubs: `dsg:Concept` requires `status` and `statement`,
and the design-graph rule mints Concepts lazily (second articulation). Each
Concept above has a design doc plus the gap as its two articulations; the
executing pass wires `ARTICULATES` from the anchoring design where the doc id
exists as a vertex, and downgrades any Concept whose second articulation turns
out to be prose-only to an Inquiry with `kind: concept`.

## Execution recipe (for the pass that runs after review)

All through `scripts/design-graph`; nothing hand-edited. The pass is authored
as one `apply` plan file (`.bbox/graphs/design/plans/<date>-gap-campaigns.jsonl`,
one JSON op per line, committed with the landing), dry-run first
(`apply <plan> --dry-run`) and reviewed, then applied: the batch is idempotent
and conflict-refusing, and lands as one generation bump. Op order inside the
plan matters because `SOURCED_FROM` requires the GapRef first and `PART_OF`
requires the Campaign first. The verb-by-verb equivalent:

1. GapRefs, one per gap:
   `create dsg:GapRef gap/<gap-id> --label "gap: <title>" --set title="<title>" --set dedupe_key="<dedupe_key>"`
   (`gap_id` derives from the id; lint checks the record exists).
2. Campaigns, one per section:
   `create dsg:Campaign campaign/<slug> --label "<name>" --set slug=<slug> --set summary="<summary>" --set status=proposed`
   then `edge campaign/<slug> dsg:SOURCED_FROM gap/<gap-id>` for each member
   gap, and `edge campaign/<slug> dsg:RELATES_TO doc/<anchor>.md --set note="anchoring design"`
   for anchors that exist as Design vertices (seed them first if the executing
   pass decides they belong in the graph; do not mint Design vertices just to
   anchor a campaign).
3. Stubs, one per gap:
   `create dsg:Inquiry inquiry/<slug> --label "<title>" --set summary="<statement>" --set status=open --set kind=<question|concept>`
   or `create dsg:Concept concept/<slug> --label "<title>" --set statement="<statement>" --set status=proposed`,
   then `edge <stub> dsg:PART_OF campaign/<slug>` and
   `edge <stub> dsg:SOURCED_FROM gap/<gap-id> --set note="pulled from the gap log"`.
4. `check` and `lint` clean; commit the plan file with the landing (one
   batch, one generation bump; or one plan per campaign if the operator
   prefers per-initiative provenance).
5. Do not touch the gap records. When a gap later closes, the closing pass
   cites the Inquiry or Concept ref in the resolution and flips the Inquiry to
   `concluded` with an `outcome`.

## What this plan does not do

- Create any vertex or edge. The schema and verb surface were exercised on
  throwaway vertices and reverted; the committed graph still holds only the
  phase 0 seed.
- Resolve, update, or re-key any gap. Gap status stays in the gap log.
- Change retrieval policy further. Embeddings are enabled graph-wide
  (operator decision, 2026-08-18): every statement-bearing text-indexed
  property (`brief`, `statement`, `synthesis`, `rationale`, `decision`,
  Module and Campaign `summary`, Inquiry `summary` and `outcome`) embeds;
  `GapRef.title` stays index-only because the gap log owns that record's
  retrieval and the mirror should not compete with it.
- Decide campaign status. Everything lands `proposed`/`open`; ratification
  is the operator's.

## Open questions for review

- Campaign 4's anchoring design: is there a single workflow-runtime design
  the three pipeline gaps hang under, or should the campaign carry no anchor?
- Whether `dsg:OpenQuestion` vertices already raised by designs should be
  re-homed under campaigns via `PART_OF` in the same pass, or left until a
  campaign is `active`.
- Whether the executing pass should also seed the anchor Design vertices
  named above (phase A miner territory) or wait for the miner.

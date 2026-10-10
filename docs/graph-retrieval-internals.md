# Graph and retrieval internals

This page explains why graph grounding exists and how the retrieval tools
compose. For operator commands, use the [Operating Guide](operating-blackbox.md).

## The grounding problem

An LLM asked a cold-start question about a codebase it has not seen this
session will answer from training priors - confidently, often wrong.
Even with BM25 transcript search, naive rerank on the top-N results
performs poorly on questions that require provenance or reasoning across
multiple entities.

Blackbox improves that by giving agents a structured retrieval surface.
The model is not expected to remember the chain. The daemon carries
canonical refs forward: search returns them and inspection confirms their
properties and provenance.

## Tool shapes and how they compose

Each retrieval tool is shaped to feed the next.

### `bbox_hybrid_search`

Output: ranked entity refs.

`bbox_hybrid_search` is the default search call. It fuses four lane
families: lexical chunk, file-level lexical, knowledge, and per-route
vector lanes.

The important behavior is that search returns entity refs, not just text.
The next call can inspect those refs without reconstructing paths or
guessing filenames.

### `bbox_inspect_entity`

Input: a canonical `<type>:<segments>` entity ref.

Output: entity properties loaded from the entity's provider store. Only
project graph vertices carry edges (their graph edges and evidence
bindings) and `recommended_next_hops`; a project file or knowledge entry
named by an evidence binding shows that binding. Other entity types have no
edge neighborhood.

For a project graph vertex, use `direction=both` for orientation, then
narrow to `direction=out` or `direction=in` once the traversal is clear;
`edge_types`, `per_type_limit` and `edge_cursor` bound the edge page. If the tool returns
`error.bad_input` with a `suggested_fix`, use the suggestion verbatim.
The ref encodes details that are not reliably reconstructible by hand.

Property detail defaults to `smart`. `property_projection.shortened` and
`omitted_keys` identify response-level reductions; select `property="content"`
(or another returned key) and follow `body.next_cursor` to recover its exact
stored value. `property_mode="full"` returns all provider properties when they
fit. These controls do not undo upstream ingestion limits. A field named
`content_preview`, including those on indexed project-file, transcript and
session entities, is an intrinsic provider preview; selecting that field reads
only the preview, even in full mode. Session `first_user_prompt` is also a
preview.

Commit inspection exposes the indexed message as `content`, so a message longer
than the smart preview has an explicit shortening marker and exact property
pages. Commit ingestion retains at most 16 KiB, including its truncation suffix.
When that suffix is present, `evidence.content_completeness="ingest_truncated"`
and `evidence.content_limitation` remain visible in summaries and exact pages.
Completing those pages recovers the stored message, not omitted original Git
bytes. Retrieval uses the pinned index generation; it does not read a checkout.

## Opening sequence

For codebase, history, or decision questions:

```text
1. bbox_knowledge(query="...")
2. bbox_hybrid_search(query="...", limit=5)
3. bbox_inspect_entity(entity_ref="...")
```

Step 1 recalls durable rules and decisions. Step 2 finds seeds. Step 3
confirms the seed's properties and provenance before answering.

Data flows forward:

- Step 2 returns canonical entity refs, so step 3 inspects them directly.
- On a project graph vertex, step 3 returns `recommended_next_hops`, so the
  next inspection follows the schema's declared edges.

## Corpus entity types

The common entity types are:

| Entity type | What it holds | Question it answers |
|---|---|---|
| `knowledge` | Rules and conventions | "what is the policy on X?" |
| `project_file` | Source and doc chunks | "where does X live?" |
| `project_file_v2` | Snapshot-scoped source and doc chunks | "which snapshot's copy of X is live?" |
| `transcript` | One content block from a session | "what did this turn say?" |
| `session` | A full agent conversation | "what was this session about?" |
| `thread` | Persistent work across sessions | "what is still active?" |
| `brofile` | Persona/model/lens triple | "which agent produced this?" |
| `commit` | Git commit metadata and touched files | "when did this change?" |
| `task` | A dispatched bro unit | "what produced this artifact?" |
| `bash_call` | One shell invocation in a transcript | "what did this command emit?" |
| `project_graph_vertex` | A project-graph vertex from an accepted or connector generation | "what does the graph say about X?" |

One Tantivy document is indexed per content block, not per session. A
long session yields many searchable blocks with independent roles and
offsets.

## Edges

Edges are directional and typed. Only project graph vertices carry them:
the typed edges their graph schema declares, and evidence bindings to
project files and knowledge entries. A project file or knowledge entry
named by an evidence binding shows that binding; no other entity has an
edge neighborhood. `symbol:` and `symbol_v2:` refs return
`error.not_found`.

Code-structure edges (AST, sections) and Git commit edges are written into
snapshot members as part of code-source publication. Retrieval does not
read them.

## Hybrid search mechanics

`bbox_hybrid_search` fuses four lane families with weighted Reciprocal
Rank Fusion, layers a rerank stage over the fused order, then applies
result-shaping passes.

### Ranked lanes

| Lane | Source | Why it exists |
|---|---|---|
| BM25 chunk | Tantivy fields such as `content`, `code_content`, `symbol`, `commit_author_name`, `path_tokens` | Precise lexical recall |
| BM25 file | Chunk scores summed per `(project_id, rel_path_hash)` over the full BM25 fetch, ranked by `sum * sqrt(count)` | Lifts files with many sparse mentions |
| Knowledge | Authorized knowledge search, resolved outside the static index | Joins fusion only when it has hits, under session visibility policy |
| Vector | HNSW over per-route embeddings | Catches paraphrases and concept matches |

In the BM25 query, `path_tokens` and `symbol` carry a 1.5 field boost, so
code-shaped queries find paths and definitions without exact prose
matches. A single-token query that looks like a code symbol adds a
`symbol_exact` clause boosted 6.0, lifting the defining chunk above
passing textual mentions.

The BM25 chunk list is truncated to the fusion fetch window, but the
file-level aggregation sums scores over the full (deeper) BM25 fetch, so
a file whose mentions are spread across many chunks still surfaces; the
lane contributes nothing when the BM25 fetch spans fewer than two
distinct files. The knowledge lane is searched separately, against the
published knowledge view for the requested project, before fusion.

Vector lanes are per route: hybrid search iterates on-disk vector
partitions with a nonzero active count and maps each back to a
configured text bucket (`code`, `docs`, `knowledge`, `transcripts`,
`git_message`, `threads`, `graph`) or visual
route, and each contributing partition becomes its own ranked list.
Unmapped partitions are skipped and reported in
`degraded.skipped_partitions`.

Graph vertex vectors (the `graph` route) carry only an entity id and a
distance, so the graph authority that the word lane composes into the
BM25 query is mirrored per hit BEFORE fusion by
`retain_authorized_graph_vectors`: the pinned policy snapshot says which
lanes embed at all and pins each lane's accepted generation, and a hit
whose vertex is no longer embed-eligible on that generation (removed,
type excluded, annotation withdrawn) drops; the per-call project scope,
`graph_source` planes, and `graph_ids` selection drop the rest. A search
with no snapshot drops every graph vector: a lane that cannot prove a
vertex readable does not serve it.

### RRF fusion

```text
score(d) = sum(weight(lane_i) / (60 + rank(d, lane_i)))
```

The smoothing constant (`RRF_K = 60.0`) keeps one strong lane from fully
suppressing items that are consistently good across several lanes.
`vector_weight` defaults to `0.6` and is clamped to `[0.0, 1.0]`; the
BM25-family lanes carry `1.0 - vector_weight`, so `0.0` is BM25-only and
`1.0` is vector-only.

### Rerank stage

After fusion, candidates pass through one of three rerank modes:

- `model` (default): the fused top-k (cross-encoder default `top_k` of
  64) are re-scored by the configured Voyage cross-encoder
  (`rerank-2.5-lite` by default); model-scored candidates land in a
  strictly higher score band than the unsent tail, then pass through the
  same heuristic type/temporal multipliers and cap as the heuristic path.
- `heuristic`: type and temporal multipliers only (confirmed knowledge
  `1.35`, imported `0.85`, doc sections `1.20`, commits `1.05`,
  transcript role user `1.10` / assistant `0.95`; temporal decay clamped
  to `[0.50, 1.25]`), capped at `1.75` over the fused score.
- `none`: raw fusion order.

A rerank API failure degrades to the heuristic path and reports
`degraded.rerank_unavailable` rather than failing the search.

### Post-processing

Applied after the rerank stage:

1. Project filter: keeps local project-file and thread refs when
   `project=` is set; project-agnostic types pass through.
2. `doc_type` filter: drops results whose type differs when set.
3. Per-file collapse: keeps only the best chunk per file.
4. Modal diversification: preserves a mix of `code_block`,
   `doc_section`, and `git_message` in the final window.

### In progress: graph vertex documents

A sibling lane is implementing
[Unified Retrieval For Reflective Graph Vertices](../design/connectors/unified-retrieval.md),
milestone M9 of the graph-native connector campaign: project-graph
vertices become word-indexed (and optionally vector-indexed) documents
under per-graph policy, with authority filters running before ranking.
That design is in progress; until it lands, graph vertices remain
reachable only through exact-ref inspection, not
`bbox_hybrid_search`.

## Schema-declared next hops

Only project-graph vertices answer `recommended_next_hops`, because their
schema knows which way each edge family runs: a vertex type in `schema.json` may declare a `hints` array
of `{edge_type, direction, label}` entries in priority order, and any
vertex type also gets a tier-0 derivation from every edge type whose
declared endpoints touch it. `bbox_inspect_entity` renders the resolved
list authored-first, then derived hops that have edges by count, then
whatever families no hint covers; each direction-aware hop prints its
label and the literal `edge_types` / `direction` arguments to pass into
the next call, and an authored hop with zero edges prints `(none)` so an
absent answer stays visible instead of vanishing. The five-hop display
cap bounds only the unauthored tail. Shape, validation codes, and the
ordering rule live in
`design/corpus/agentic-corpus/reflective-project-graph.md`.

## Provider behavior

Providers dispatch through vendor CLIs by lane: the claude CLI lane for
Claude and the Anthropic-compatible endpoints, the standalone
`bro-harness` binary for the remaining harness providers. The code-owned
catalog in `crates/bro-core/src/provider.rs` is the authority: `codex` is
a serde alias to `brodex`, and the Gemini lane is removed. See
[Provider & Agent Surfaces](../PROJECT.md#provider--agent-surfaces)
rather than re-inventorying providers here.

## System memories

The detailed agent runbooks are runtime-loaded system memories fetched on
demand:

```text
bbox_knowledge(query="sm-agentic-opening-sequence")
bbox_knowledge(query="sm-transcript-retrieval")
bbox_knowledge(query="sm-persistence-taxonomy")
```

They stay out of provider files until needed, which keeps hot context
smaller.

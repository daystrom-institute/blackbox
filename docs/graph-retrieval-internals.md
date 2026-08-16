# Graph and retrieval internals

This page explains why graph grounding exists and how the retrieval tools
compose. For operator commands, use the [Operating Guide](operating-blackbox.md).

## The grounding problem

An LLM asked a cold-start question about a codebase it has not seen this
session will answer from training priors - confidently, often wrong.
Even with BM25 transcript search, naive rerank on the top-N results
performs poorly on questions that require provenance or reasoning across
multiple entities.

Blackbox improves that by giving agents a structured traversal surface.
The model is not expected to remember the chain. The daemon carries refs,
edges, path IDs, and evidence bundles forward.

## Tool shapes and how they compose

Each graph tool is shaped to feed the next.

### `bbox_hybrid_search` and `bbox_discover_seed_entities`

Output: ranked entity refs, each with a `notable_edges` preview.

`bbox_hybrid_search` is the default search call. It fuses four lane
families: lexical chunk, file-level lexical, knowledge, and per-route
vector lanes. `bbox_discover_seed_entities` is the same orientation
pattern with notable edges rendered inline for each seed.

The important behavior is that search returns graph refs, not just text.
The next call can inspect those refs without reconstructing paths or
guessing filenames.

### `bbox_inspect_entity`

Input: a canonical `<type>:<segments>` entity ref.

Output: entity properties, filtered edges, and
`recommended_next_hops`.

Use `direction=both` for orientation, then narrow to `direction=out` or
`direction=in` once the traversal is clear. If the tool returns
`error.bad_input` with a `suggested_fix`, use the suggestion verbatim.
The ref encodes details that are not reliably reconstructible by hand.

### `bbox_find_paths`

Input: a source ref, a destination (an exact `to` ref or a `to_type`
entity type, at least one of which is required), and optional edge
filters. A call with neither destination is refused with
`error.bad_input`; it is a malformed call, not an empty neighborhood.

Over project graphs, pass the logical `to_type="project_graph_vertex"`
under any visibility. Under `own` and `all` it also matches the
`provisional_project_graph_vertex` refs that a working generation
materializes as, so the caller never has to know the overlay type name.
Pass `to_type="provisional_project_graph_vertex"` only to target overlay
vertices exclusively.

Output: direction-preserving paths plus opaque `path_ids`. Each step
carries its own direction label, so backward hops come back as `in`
steps next to forward `out` steps; state them as returned.

`path_ids` are server-held handles for validated traversals. Pass them
to `bbox_bundle_evidence`; do not restate the path from memory.

### `bbox_bundle_evidence`

Input: selected refs and `path_ids`.

Output: a structured bundle with previews, refs, and validated path text.
Use it before answering when the answer depends on multi-hop traversal.

## Opening sequence

For codebase, history, or decision questions:

```text
1. bbox_describe_schema
2. bbox_hybrid_search(query="...", limit=5)
3. bbox_inspect_entity(entity_ref="...")
4. bbox_find_paths(from="...", to_type="...")
5. bbox_bundle_evidence(entity_refs=[...], path_ids=[...])
```

Step 1 is once per session. Step 4 is only needed for multi-hop
questions. Step 5 is the evidence close.

Data flows forward:

- Step 2 returns `notable_edges`, so the agent has a next-hop menu.
- Step 3 returns `recommended_next_hops`, so traversal is ranked by the index.
- Step 4 returns `path_ids`, so evidence does not depend on model memory.
- Step 5 packages the refs and paths into a reviewable answer kit.

## Corpus entity types

`bbox_describe_schema` returns live population counts. The common entity
types are:

| Entity type | What it holds | Question it answers |
|---|---|---|
| `knowledge` | Rules, decisions, conventions | "what is the policy on X?" |
| `project_file` | Source and doc chunks | "where does X live?" |
| `project_file_v2` | Snapshot-scoped source and doc chunks | "which snapshot's copy of X is live?" |
| `transcript` | One content block from a session | "what did this turn say?" |
| `session` | A full agent conversation | "what was this session about?" |
| `thread` | Persistent work across sessions | "what is still active?" |
| `note` | Structured side-channel records | "what did the executor flag?" |
| `symbol` | Named code symbols | "what calls or defines this?" |
| `symbol_v2` | Snapshot-scoped code symbols | "which definition is live?" |
| `brofile` | Persona/model/lens triple | "which agent produced this?" |
| `whiteboard` | Multi-agent deliberation state | "what is on the board?" |
| `commit` | Git commit metadata and touched files | "when did this change?" |
| `task` | A dispatched bro unit | "what produced this artifact?" |
| `bash_call` | One shell invocation in a transcript | "what did this command emit?" |
| `roadmap_item` | Prospective work items | "what is planned or deferred?" |
| `project_graph_vertex` | A project-graph vertex (provisional refs cover working generations) | "what does the graph say about X?" |

One Tantivy document is indexed per content block, not per session. A
long session yields many searchable blocks with independent roles and
offsets.

## Edge families

Edges are directional and typed.

| Family | Edge kinds |
|---|---|
| Structural | `IN_FILE`, `IN_SESSION`, `NEXT_SECTION`, `NEXT_CHUNK`, `PREV_CHUNK`, `THREAD_HAS_SESSION`, `THREAD_SPAWNED_FROM`, `THREAD_BLOCKED_BY`, `THREAD_RELATES_TO`, `THREAD_SUBSUMES` |
| AST | `DEFINED_IN`, `CONTAINS_SYMBOL`, `CALLS`, `USES_TYPE`, `HAS_FIELD`, `IMPLEMENTS_TRAIT` |
| Knowledge | `SUPERSEDES`, `DERIVED_FROM`, `Contradicts`, `RelatesTo`, `TensionWith`, `Supports`, `DependsOn`, `REFERENCES`, `KNOWLEDGE_FROM_SESSION`, `KNOWLEDGE_FROM_BOARD` |
| Provenance | `SESSION_USED_BROFILE`, `ARC_USED_BROFILE`, `ARC_OPENED_BOARD`, `NOTE_FROM_SESSION`, `NOTE_IN_THREAD`, `NOTE_FROM_TASK`, `TASK_PRODUCED_NOTE` |
| Git | `COMMIT_PARENT`, `COMMIT_TOUCHED_FILE`, `COMMIT_PRODUCED_BY_ARC` |
| Roadmap | `ROADMAP_SPAWNS`, `ROADMAP_DEFERRED_FROM`, `ROADMAP_DESIGNED_IN`, `ROADMAP_DEPENDS_ON`, `ROADMAP_BLOCKED_BY`, `ROADMAP_SUPERSEDES`, `ROADMAP_SUBSUMES`, `ROADMAP_RELATED_TO` |
| Format-specific | `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `DESCRIBES`, `ON_PAGE`, `FIGURE_OF`, `TABLE_OF` |
| Tool-call | `EDITED_FILE`, `EDITED_BY_SESSION`, `READ_FILE`, `RAN_BASH` |

`bbox_describe_schema`'s edge catalog is currently narrower than this
table (no Roadmap family; Knowledge limited to `SUPERSEDES`,
`DERIVED_FROM`, `Contradicts`, `KNOWLEDGE_FROM_SESSION`,
`KNOWLEDGE_FROM_BOARD`), so its output can omit families listed here.

The EdgeIndex is built from per-project JSONL sidecars plus live
knowledge, thread, note, and roadmap stores, with virtual edges for
tasks and tool calls.

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
distinct files. The knowledge lane is searched separately because
provisional-visibility policy must be resolved against the caller's
session before fusion.

Vector lanes are per route: hybrid search iterates on-disk vector
partitions with a nonzero active count and maps each back to a
configured text bucket (`code`, `docs`, `knowledge`, `transcripts`,
`git_message`, `notes`, `threads`, `agent_manifest`) or visual route,
and each contributing partition becomes its own ranked list. Unmapped
partitions are skipped and reported in `degraded.skipped_partitions`.

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
reachable only through exact-ref inspection and traversal, not
`bbox_hybrid_search`.

## Provider behavior

The dispatch plane contains zero provider CLIs; providers dispatch
through the standalone `bro-harness` binary. The code-owned catalog in
`crates/bro-core/src/provider.rs` is the authority: `claude` survives
only as a serde alias to `glm`, `codex` as an alias to `brodex`, and
the Gemini lane is removed. See
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

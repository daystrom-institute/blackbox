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

`bbox_hybrid_search` is the default search call. It blends lexical,
file-level, and vector lanes. `bbox_discover_seed_entities` is the same
orientation pattern with notable edges rendered inline for each seed.

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

Input: a source ref, optional destination ref or destination type, and
optional edge filters.

Output: direction-preserving paths plus opaque `path_ids`.

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
| `transcript` | One content block from a session | "what did this turn say?" |
| `session` | A full agent conversation | "what was this session about?" |
| `thread` | Persistent work across sessions | "what is still active?" |
| `note` | Structured side-channel records | "what did the executor flag?" |
| `symbol` | Named code symbols | "what calls or defines this?" |
| `brofile` | Persona/model/lens triple | "which agent produced this?" |
| `whiteboard` | Multi-agent deliberation state | "what is on the board?" |
| `commit` | Git commit metadata and touched files | "when did this change?" |
| `task` | A dispatched bro unit | "what produced this artifact?" |
| `bash_call` | One shell invocation in a transcript | "what did this command emit?" |

One Tantivy document is indexed per content block, not per session. A
long session yields many searchable blocks with independent roles and
offsets.

## Edge families

Edges are directional and typed.

| Family | Edge kinds |
|---|---|
| Structural | `IN_FILE`, `IN_SESSION`, `NEXT_SECTION`, `NEXT_CHUNK`, `PREV_CHUNK`, `THREAD_HAS_SESSION` |
| AST | `DEFINED_IN`, `CONTAINS_SYMBOL`, `CALLS`, `USES_TYPE`, `HAS_FIELD`, `IMPLEMENTS_TRAIT` |
| Knowledge | `SUPERSEDES`, `DERIVED_FROM`, `CONTRADICTS`, `KNOWLEDGE_FROM_SESSION`, `KNOWLEDGE_FROM_BOARD` |
| Provenance | `SESSION_USED_BROFILE`, `ARC_USED_BROFILE`, `ARC_OPENED_BOARD`, `NOTE_FROM_SESSION`, `NOTE_IN_THREAD`, `NOTE_FROM_TASK`, `TASK_PRODUCED_NOTE` |
| Git | `COMMIT_PARENT`, `COMMIT_TOUCHED_FILE`, `COMMIT_PRODUCED_BY_ARC` |
| Format-specific | `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `DESCRIBES`, `ON_PAGE`, `FIGURE_OF`, `TABLE_OF` |
| Tool-call | `EDITED_FILE`, `EDITED_BY_SESSION`, `READ_FILE`, `RAN_BASH` |

The EdgeIndex is built from per-project JSONL sidecars plus live
knowledge, thread, and note stores.

## Hybrid search mechanics

`bbox_hybrid_search` fuses three ranked lists with Reciprocal Rank
Fusion, then applies result-shaping passes.

### Ranked lanes

| Lane | Source | Why it exists |
|---|---|---|
| BM25 chunk | Tantivy fields such as `content`, `code_content`, `symbol`, `path_tokens` | Precise lexical recall |
| BM25 file | Aggregate score per file | Lifts files with many sparse mentions |
| Vector | HNSW over route-specific embeddings | Catches paraphrases and concept matches |

`path_tokens` and `symbol` are boosted so code-shaped queries can find
paths and definitions without exact prose matches.

### RRF fusion

```text
score(d) = sum(1 / (60 + rank(d, lane_i)))
```

The smoothing constant keeps one strong lane from fully suppressing
items that are consistently good across several lanes.

### Post-processing

Applied after fusion:

1. Project filter: keeps local project-file refs when `project=` is set.
2. Per-file collapse: avoids returning many chunks from one file.
3. Modal diversification: preserves a mix of code, docs, and commits.
4. Symbol exact boost: raises defining chunks for exact symbol queries.

## Provider behavior

Provider reliability depends on whether the CLI reads rendered guidance
and uses MCP tools.

| Provider | Honors imports | Uses bbox tools | Notes |
|---|---|---|---|
| `claude` | yes | yes | Strongest cold-start grounding |
| `codex` | yes | yes | Good quality, usually higher latency |
| `gemini` | yes | expected | Renders to `GEMINI.md` |
| `glm` / `deepseek` via opencode | no | usually falls back to grep/read | Import-following gap reduces grounding |

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

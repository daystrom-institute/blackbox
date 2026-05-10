# Internals — search, indexing, and the graph

How blackbox represents knowledge, how retrieval works, and what
happens under the hood when you call `bbox_hybrid_search` or
`bbox_inspect_entity`.

## Corpus entity types

The corpus is a typed graph. `bbox_describe_schema` returns the live
population counts; the canonical types are:

| Entity type | What it holds | Primary question answered |
|---|---|---|
| `knowledge` | Rules, decisions, conventions | "what's the policy on X?" |
| `project_file` | Source / doc chunks (indexed at 10KB per chunk) | "where does X live in the code?" |
| `transcript` | One content block from one agent session | "what did this turn say?" |
| `session` | A full agent conversation | "what was that session about?" |
| `thread` | Persistent investigation spanning sessions | "what's the active arc for X?" |
| `note` | Side-channel records (dispute/done/blocked/etc) | "what did the executor flag?" |
| `symbol` | Named code symbols | "what calls this function?" |
| `brofile` | Persona + model + lens triple | "which agent produced this?" |
| `whiteboard` | Multi-agent deliberation surface | "what's the board state?" |
| `commit` | Git commits with parent + touched-file edges | "when did this change?" |
| `task` (virtual) | A `bro_exec` dispatch unit | "what produced this artifact?" |
| `bash_call` (virtual) | One shell invocation in a transcript | "what did this command emit?" |

## Edge families

Edges are directional and typed. `bbox_find_paths` follows them;
`bbox_inspect_entity` returns them filtered by `edge_types` and
`direction`.

| Family | Edge kinds |
|---|---|
| **Structural** | IN_FILE, IN_SESSION, NEXT_SECTION, NEXT_CHUNK, PREV_CHUNK, THREAD_HAS_SESSION |
| **AST** | DEFINED_IN, CONTAINS_SYMBOL, CALLS, USES_TYPE, HAS_FIELD, IMPLEMENTS_TRAIT |
| **Knowledge** | SUPERSEDES, DERIVED_FROM, CONTRADICTS, KNOWLEDGE_FROM_SESSION, KNOWLEDGE_FROM_BOARD |
| **Provenance** | SESSION_USED_BROFILE, ARC_USED_BROFILE, ARC_OPENED_BOARD, NOTE_FROM_SESSION, NOTE_IN_THREAD, NOTE_FROM_TASK, TASK_PRODUCED_NOTE |
| **Git** | COMMIT_PARENT, COMMIT_TOUCHED_FILE, COMMIT_PRODUCED_BY_ARC |
| **Format-specific** | LINKS_TO_FILE, LINKS_TO_SECTION, DESCRIBES, ON_PAGE, FIGURE_OF, TABLE_OF |
| **Tool-call** | EDITED_FILE, EDITED_BY_SESSION, READ_FILE, RAN_BASH |

## Agentic opening sequence

The standard pattern for any task that touches the codebase or prior
decisions:

```
1. bbox_describe_schema           # orient — entity types + edge families (once per session)
2. bbox_hybrid_search(q, k=5)     # seed — mixed-modal results with notable_edges
3. bbox_inspect_entity(ref)       # confirm — properties + edges in one call
4. bbox_find_paths(from, to_*)    # traverse — BFS chains (when multi-hop)
5. bbox_bundle_evidence(...)      # close — package refs + path_ids before answering
```

Full runbook: `bbox_knowledge(query="sm-agentic-opening-sequence")`.

## Hybrid search

`bbox_hybrid_search` fuses three ranked lists via Reciprocal Rank
Fusion (RRF), then applies four post-processing passes.

### Ranked lists

**1. bm25** — chunk-level Tantivy BM25 over multiple fields.
Field boosts:

| Field | Boost | Notes |
|---|---|---|
| `path_tokens` | 1.5× | Code tokenizer: splits on `/_-.:>` plus CamelCase |
| `symbol` | 1.5× | Named code symbols |
| `content` | 1.0× | Full text of the chunk |
| `code_content` | 1.0× | Source code blocks |
| `commit_author_name` | 1.0× | Git author |

**2. bm25_file** — file-level aggregation. Sums per-chunk BM25 scores
for all chunks of the same file, then weights by `sum × √chunk_count`.
Lifts high-coverage files (e.g. a STATUS.md with 21 sparse mentions)
that would otherwise be invisible to per-chunk ranking.

**3. vector** — HNSW approximate nearest neighbor over Voyage embeddings
(1024d, `voyage-code-3` by default). Default RRF weight: 0.6 vector /
0.4 BM25. Override via `vector_weight` parameter: `0.0` for BM25-only,
`1.0` for vector-only.

### RRF fusion

The three lists are combined with RRF (k=60 smoothing constant):

```
score(d) = Σ  1 / (k + rank(d, list_i))
```

### Post-processing passes (in order)

1. **Project filter** — when `project=<path or project_id>` is passed,
   drop `project_file` refs from other projects. Commits, knowledge, and
   transcripts pass through. This cuts cross-repo keyword pollution
   (e.g. "voyage" returning `erlang-test/voyage.ex` above
   `transcript-search/src/embed/voyage.rs`).

2. **Per-file collapse** — only the highest-scoring chunk per file
   survives. Mirrors an AgenticTools diversity-by-file pass.

3. **Modal diversification** — guarantees at least one `code_block`,
   `doc_section`, and `git_message` in top-N when the fetch set contains
   them. Prevents a query like "triad implementation" from returning 10
   doc chunks with the defining `.ex` file invisible.

4. **Symbol_exact boost** — single-token queries (snake_case, CamelCase,
   dotted paths) add a SHOULD clause against the `symbol_exact` field
   with 6× boost, lifting the defining chunk above documents that only
   mention the symbol in prose.

`bbox_discover_seed_entities` runs the same pipeline but also renders
`notable_edges` for each result — use it when the next step is
`bbox_inspect_entity` and you want pre-vetted traversal hops.

## Embedding pipeline

Voyage embeddings (`voyage-code-3`, 1024d) power the vector lane.
The daemon runs a per-route async queue: one worker per route with
debounce, batching, and retry.

### Routes

| Route | What's embedded | Default provider |
|---|---|---|
| `code` | Source-file code chunks | voyage / voyage-code-3 |
| `docs` | Source-file doc chunks (markdown, comments) | voyage / voyage-code-3 |
| `git_message` | Commit subject + body | voyage / voyage-code-3 |
| `knowledge` | Knowledge-store entries | voyage / voyage-code-3 |
| `notes` | Side-channel notes | voyage / voyage-code-3 |
| `transcripts` | Transcript event blocks | voyage / voyage-code-3 |

Each route persists its own HNSW partition keyed on
`(provider, model, dimensions)`. Switching provider or model requires a
full re-embed of that route — existing partitions for other routes are
unaffected.

### Batch caps

The queue caps at **64 documents** or **80KB total** per Voyage request
(under the 128 doc / 120KB Voyage limits). Without this cap, a restart
that re-fills the queue with thousands of pending chunks would send one
oversized request, get rejected, retry 3×, and drop the entire batch.

### Ollama fallback

Set a route to `ollama` in `~/.config/blackbox/embed.toml` to use a
local `nomic-embed-text` endpoint (768d) without an API key. Mixing
Voyage and Ollama routes is fine; each maintains its own HNSW partition.
Dimensions must not change within a partition — switching from 768d to
1024d on the same route requires `bbox_reembed` to rebuild.

### Status check

```
bbox_embed_status()
```

Per-route fields: `available`, `provider`, `model`, `dim`,
`indexed_count`, `queue_depth`, `retried_count`, `last_error`. A healthy
daemon shows `available: true` and `last_error: null` on all routes.
Non-zero `queue_depth` is normal during a reindex; watch for it to drain.

## Tantivy index and schema versioning

Blackbox indexes one Tantivy document per content block (not per
session). This enables role-based filtering and precise excerpt
generation. Schema version is tracked via `INDEX_SCHEMA_VERSION` in
`src/index/mod.rs`.

On daemon start: if the stored schema marker doesn't match the binary's
version, the index is dropped and rebuilt automatically. A full rebuild
on a 1M-doc corpus takes **5–7 minutes**. The EdgeIndex rebuild fires
automatically via a watcher thread after the doc count stabilizes
(~6 seconds after reindex completes).

### Schema version history

| Version tag | Change |
|---|---|
| `agentic-corpus-g1` | Initial agentic schema |
| `agentic-corpus-g2-path-tokens` | Tokenized `path_tokens` field |
| `agentic-corpus-g3-commit-subject-tokens` | `path_tokens` from commit subject so commits rank alongside project files |
| `agentic-corpus-g4-elixir-symbols` | Elixir symbol extraction via tree-sitter |
| `agentic-corpus-g5-symbol-tokenized` | `symbol` field switched to code_tokenizer so `Substrate.TriadClosure` matches both camelCase and snake_case queries |

## EdgeIndex

The EdgeIndex is a graph projection over the tantivy substrate. It's
built from the JSONL edge sidecars under
`~/.local/state/blackbox/edges/<project_id>.jsonl` plus in-memory edges
from the live knowledge/thread/note stores. The watcher thread
auto-triggers a rebuild when the tantivy doc count grows.

Manual rebuild: `bbox_edge_compact` compresses a project's sidecar;
`bbox_reindex(full=true)` forces a full tantivy rebuild which cascades
to EdgeIndex.

## Provider integration

How well each CLI provider follows the agentic opening sequence when
given a cold-start question:

| Provider | Honors AGENTS.md @-imports | Uses bbox_* tools | Notes |
|---|---|---|---|
| `claude` (Opus 4.7) | ✅ | ✅ first-class | Best cold-start reliability; follows the 5-step loop naturally |
| `codex` (gpt-5.5) | ✅ | ✅ first-class | Quality high; latency tends 2× of Claude |
| `gemini` (gemini-3.1-pro) | ✅ | Untested | Renders to GEMINI.md; expected to mirror Claude |
| `glm` / `deepseek` (via opencode) | ❌ | Falls back to grep/read | opencode doesn't follow `@/path/...` @-imports in AGENTS.md; tracked in deferred thread |

## System memories

Code-embedded runbooks pulled on demand via `bbox_knowledge(query="sm-<id>")`.
Not rendered into provider files — kept cold and fetched when needed.

| ID | Topic |
|---|---|
| `sm-agentic-opening-sequence` | 5-step grounding pattern |
| `sm-transcript-retrieval` | search / cite / context / session ladders |
| `sm-persistence-taxonomy` | learn vs decide vs remember vs pin |
| `sm-render-lifecycle` | Render → review → revoke flow |
| `sm-scoped-pins` | Active-arc context pins |
| `sm-create-etiquette` | List-before-create dedupe hygiene |
| `sm-side-channel-notes` | dispute / assumption / surprise / followup / blocked / learned / done |
| `sm-rule-packets` | Compile reusable judges from examples |
| `sm-workflow-orchestration` | JSON workflow specs |
| `sm-bro-dispatch-patterns` | bro_exec / bro_resume usage |
| `sm-whiteboards` | Multi-agent deliberation boards |
| `sm-design-packets` | Multi-domain packet design |
| `sm-auth-packets` | Authorization / policy packets |
| `sm-review-packets` | Review-style packets |
| `sm-refactor` | Refactor mechanization runbook |
| `sm-refactor-java` | Java-specific refactor plan kinds |

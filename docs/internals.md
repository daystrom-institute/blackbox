# Internals — graph grounding, search, and indexing

## The grounding problem

An LLM asked a cold-start question about a codebase it hasn't seen this
session will answer from training priors — confidently, often wrong.
Even with BM25 transcript search, naive rerank on the top-N results
achieves around **23% recall** on questions that require tracing
provenance or following a reasoning chain across multiple entities.

Blackbox's graph layer raises that to around **97%** on the same probe
set. The improvement doesn't come from a better model — it comes from
giving agents a structured traversal surface and a sequence of tool
calls with interlocking shapes that carry evidence forward rather than
starting each step from scratch.

---

## Tool shapes and how they compose

Each tool in the opening sequence was designed to feed the next. The
output shape of step N is the required input shape of step N+1.

### `bbox_hybrid_search` / `bbox_discover_seed_entities`

**Output shape:** a ranked list of entity refs, each with a
`notable_edges` preview — the highest-signal outgoing and incoming edges
for that entity, prioritized semantic-first.

**Why this shape:** the agent doesn't have to run a second call to know
what to inspect next. The notable_edges are a pre-vetted traversal menu.
`bbox_discover_seed_entities` is the same search but with notable_edges
rendered inline for every result — use it when the next step is
`bbox_inspect_entity` and you want the hop targets surfaced before you
commit to one.

The vector lane catches paraphrases — if the user's query wording
doesn't match the corpus's wording, cosine similarity finds the
canonical entity anyway. This is why the top seed is canonical for the
query even when lexical matching would miss it.

### `bbox_inspect_entity`

**Input:** a canonical `<type>:<segments>` entity ref  
**Output:** entity properties, targeted edges filtered by `edge_types`
and `direction`, and a `recommended_next_hops` list ordered
semantic-first (the hops most likely to carry meaningful context, not
just structurally reachable nodes)

**Why this shape:** one call returns both properties and edges.
`recommended_next_hops` means the agent doesn't have to reason about
which edges to follow — the index pre-ranks them. The `edge_types` and
`direction` parameters exist so you can narrow from the orientation
sweep (`direction=both`) to targeted reads (`direction=out,
edge_types=SUPERSEDES`) as you understand the subgraph.

Critical: entity refs are canonical strings. When a call returns
`error.bad_input` with a `suggested_fix`, use the suggestion verbatim —
don't guess a corrected ref. The ref format encodes path hashes and
chunk positions that aren't reconstructible by inspection.

### `bbox_find_paths`

**Input:** a `from` entity ref, optional `to` ref or `to_type` target,
optional `edge_types` filter  
**Output:** BFS chains with **path_ids** — server-side identifiers for
the validated traversal results

**Why this shape:** path_ids are the key. They are opaque handles that
the server holds — passing them to `bbox_bundle_evidence` lets you cite
a multi-hop reasoning chain without reconstructing the path text from
memory (which would be subject to hallucination). Direction is
preserved; do not invert edges from memory.

Skip this step entirely when the question is single-hop. It's only
needed when the answer requires following a chain across entity
boundaries.

### `bbox_bundle_evidence`

**Input:** selected entity refs + `path_ids` from `bbox_find_paths`  
**Output:** a structured evidence bundle with cited refs and validated
path text

**Why this shape:** closing the loop. Before giving an answer that
depends on graph traversal, packaging the evidence bundle lets the
answer be re-queried, cited, and verified. It also surfaces the
evidence to a human reviewer without them having to re-run the walk.

---

## The agentic opening sequence

For any task that touches the codebase, prior decisions, or
conversational history, run this sequence before falling back to
filesystem search or training-prior answers:

```
1. bbox_describe_schema           # orient — entity types + edge families (once per session)
2. bbox_hybrid_search(q, k=5)     # seed — ranked results with notable_edges
3. bbox_inspect_entity(ref)       # confirm — properties + edges + recommended_next_hops
4. bbox_find_paths(from, to_*)    # traverse — direction-preserving BFS, returns path_ids
5. bbox_bundle_evidence(...)      # close — package entity refs + path_ids before answering
```

Step 1 is one-time per session — cache the schema mentally, don't
repeat it on every query. Step 4 is conditional — skip it when the
answer is single-hop. Step 5 is the close-the-loop write.

**What makes this work is the data flowing forward:**

- Step 2 returns `notable_edges` → you know which entity to inspect
  without guessing
- Step 3 returns `recommended_next_hops` → you know which edges are
  semantically relevant without enumerating all edges
- Step 4 returns `path_ids` → you pass them directly to step 5, not
  reconstructed text; the server validates the chain
- Step 5 packages refs + path_ids → the answer is cite-able and the
  evidence is round-trippable

The failure mode when this sequence is skipped: agents use training
priors to guess file names, function signatures, and decision rationale.
They're confident and wrong at a rate that makes them unreliable for
any task requiring accurate provenance.

Full runbook with pattern recipes (where/what/who/why/how/replacement/
historical/impact questions): `bbox_knowledge(query="sm-agentic-opening-sequence")`.

---

## The corpus: entity types

The graph substrate. `bbox_describe_schema` returns live population
counts for all types.

| Entity type | What it holds | Question it answers |
|---|---|---|
| `knowledge` | Rules, decisions, conventions | "what's the policy on X?" |
| `project_file` | Source / doc chunks (up to 10KB per chunk) | "where does X live in the code?" |
| `transcript` | One content block from one agent session | "what did this turn say?" |
| `session` | A full agent conversation | "what was this session about?" |
| `thread` | Persistent investigation spanning sessions | "what's the active arc for X?" |
| `note` | Side-channel records (dispute/done/blocked/etc) | "what did the executor flag?" |
| `symbol` | Named code symbols | "what calls this function?" |
| `brofile` | Persona + model + lens triple | "which agent produced this?" |
| `whiteboard` | Multi-agent deliberation surface | "what's the board state?" |
| `commit` | Git commits with parent + touched-file edges | "when did this change?" |
| `task` (virtual) | A `bro_exec` dispatch unit | "what produced this artifact?" |
| `bash_call` (virtual) | One shell invocation in a transcript | "what did this command emit?" |

One Tantivy document is indexed per content block — not per session.
This enables role-based filtering (`role=user` vs `role=assistant`) and
precise excerpt generation. Sessions with 50 turns produce 50+ documents,
each independently searchable.

## Edge families

Edges are directional and typed. `bbox_find_paths` follows them;
`bbox_inspect_entity` returns them filtered by `edge_types` and
`direction`. Use `direction=out` or `direction=in` once you know what
you're looking for — `direction=both` is for initial orientation only.

| Family | Edge kinds |
|---|---|
| **Structural** | IN_FILE, IN_SESSION, NEXT_SECTION, NEXT_CHUNK, PREV_CHUNK, THREAD_HAS_SESSION |
| **AST** | DEFINED_IN, CONTAINS_SYMBOL, CALLS, USES_TYPE, HAS_FIELD, IMPLEMENTS_TRAIT |
| **Knowledge** | SUPERSEDES, DERIVED_FROM, CONTRADICTS, KNOWLEDGE_FROM_SESSION, KNOWLEDGE_FROM_BOARD |
| **Provenance** | SESSION_USED_BROFILE, ARC_USED_BROFILE, ARC_OPENED_BOARD, NOTE_FROM_SESSION, NOTE_IN_THREAD, NOTE_FROM_TASK, TASK_PRODUCED_NOTE |
| **Git** | COMMIT_PARENT, COMMIT_TOUCHED_FILE, COMMIT_PRODUCED_BY_ARC |
| **Format-specific** | LINKS_TO_FILE, LINKS_TO_SECTION, DESCRIBES, ON_PAGE, FIGURE_OF, TABLE_OF |
| **Tool-call** | EDITED_FILE, EDITED_BY_SESSION, READ_FILE, RAN_BASH |

The EdgeIndex is built from per-project JSONL edge sidecars
(`~/.local/state/blackbox/edges/<project_id>.jsonl`) plus in-memory
edges from the live knowledge/thread/note stores. A watcher thread
auto-triggers a rebuild when the tantivy doc count grows.

---

## Hybrid search mechanics

`bbox_hybrid_search` fuses three ranked lists via Reciprocal Rank
Fusion (RRF), then applies four post-processing passes.

### The three lists

**BM25 (chunk-level)** — Tantivy BM25 over indexed fields, with boosts:

| Field | Boost | Notes |
|---|---|---|
| `path_tokens` | 1.5× | Code tokenizer: splits on `/_-.:>` plus CamelCase |
| `symbol` | 1.5× | Named code symbols |
| `content` | 1.0× | Full chunk text |
| `code_content` | 1.0× | Source code |

**BM25-file (file-level aggregation)** — sums per-chunk BM25 scores
across all chunks of a file, weighted by `sum × √chunk_count`. Lifts
files with many sparse mentions (e.g. a STATUS.md with 21 references to
a topic) that per-chunk ranking buries.

**Vector** — HNSW approximate nearest neighbor over Voyage embeddings
(`voyage-code-3`, 1024d). Default RRF weight: 0.6 vector / 0.4 BM25.
Override with `vector_weight`: `0.0` for BM25-only, `1.0` for
vector-only.

### RRF fusion

```
score(d) = Σ  1 / (60 + rank(d, list_i))
```

The k=60 smoothing constant prevents high-ranked items in one list from
completely dominating results that appear consistently across all three.

### Post-processing passes

Applied in order after fusion:

1. **Project filter** — `project=<path or project_id>` drops
   `project_file` refs from other projects while passing commits,
   knowledge, and transcripts through. Without this, a query for
   "voyage" in the transcript-search repo surfaces `erlang-test/voyage.ex`
   ahead of `transcript-search/src/embed/voyage.rs`.

2. **Per-file collapse** — only the highest-scoring chunk per file
   survives. Ensures result diversity across files rather than returning
   10 chunks from the same large file.

3. **Modal diversification** — guarantees at least one `code_block`,
   `doc_section`, and `git_message` in top-N when the fetch set contains
   them. Without this, a query like "triad implementation" can return 10
   doc chunks with the defining `.ex` source file invisible.

4. **Symbol_exact boost** — single-token queries (snake_case, CamelCase,
   dotted paths) add a SHOULD clause on `symbol_exact` with 6× boost,
   lifting the defining chunk above documents that only mention the
   symbol in prose.

---

## Embedding pipeline

Voyage embeddings power the vector lane. The daemon runs one async
worker per route with debounce, batching, and retry.

### Routes

| Route | What's embedded | Provider |
|---|---|---|
| `code` | Source-file code chunks | voyage-code-3 |
| `docs` | Source-file doc chunks (markdown, comments) | voyage-code-3 |
| `git_message` | Commit subject + body | voyage-code-3 |
| `knowledge` | Knowledge-store entries | voyage-code-3 |
| `notes` | Side-channel notes | voyage-code-3 |
| `transcripts` | Transcript event blocks | voyage-code-3 |

Each route persists its own HNSW partition keyed on
`(provider, model, dimensions)`. Switching a route's provider or model
invalidates that partition; the others are unaffected.

### Batch cap

64 documents or 80KB total per Voyage request (Voyage limits: 128 / 120KB).
The cap prevents a restart from flushing thousands of queued chunks in
one oversized request, getting rejected, retrying 3×, and silently
dropping the batch.

### Ollama fallback

Route to `ollama` in `~/.config/blackbox/embed.toml` to use
`nomic-embed-text` (768d) without an API key. Mixed-provider setups work;
each maintains its own HNSW partition. Changing dimensions within a
route requires `bbox_reembed` to rebuild.

### Status

```
bbox_embed_status()
```

Fields per route: `available`, `provider`, `model`, `dim`,
`indexed_count`, `queue_depth`, `retried_count`, `last_error`. Healthy:
`available: true`, `last_error: null`. Non-zero `queue_depth` is normal
during reindex; watch for it to drain.

---

## Tantivy index and schema versioning

`INDEX_SCHEMA_VERSION` in `src/index/mod.rs` gates schema compatibility.
On startup, if the stored marker doesn't match the binary's version, the
index drops and rebuilds automatically (~5–7 minutes for 1M docs). The
EdgeIndex watcher fires ~6 seconds after the doc count stabilizes.

Schema version history:

| Tag | Change |
|---|---|
| `agentic-corpus-g1` | Initial agentic schema |
| `agentic-corpus-g2-path-tokens` | Tokenized `path_tokens` field |
| `agentic-corpus-g3-commit-subject-tokens` | `path_tokens` from commit subjects so commits rank alongside project files |
| `agentic-corpus-g4-elixir-symbols` | Elixir symbol extraction via tree-sitter |
| `agentic-corpus-g5-symbol-tokenized` | `symbol` switched to code_tokenizer — `Substrate.TriadClosure` now matches both camelCase and snake_case queries |

---

## Provider integration

How well each CLI follows the agentic opening sequence from a cold start:

| Provider | Honors @-imports | Uses bbox_* | Notes |
|---|---|---|---|
| `claude` (Opus 4.7) | ✅ | ✅ first-class | Best cold-start reliability; follows the 5-step loop naturally |
| `codex` (gpt-5.5) | ✅ | ✅ first-class | Quality high; latency typically 2× Claude |
| `gemini` (gemini-3.1-pro) | ✅ | Untested | Renders to GEMINI.md; expected to mirror Claude |
| `glm` / `deepseek` (opencode) | ❌ | Falls back to grep/read | opencode doesn't follow `@/path/...` @-imports in AGENTS.md |

The opencode gap means GLM and DeepSeek bros operate without graph
grounding, reducing them to filesystem search. Tracked in the deferred
items thread.

---

## System memories

Code-embedded runbooks pulled on demand — not rendered into provider
files, so they don't bloat every session's context. Fetch with
`bbox_knowledge(query="sm-<id>")`.

| ID | Topic |
|---|---|
| `sm-agentic-opening-sequence` | Full 5-step pattern with recipes for where/what/who/why/how questions |
| `sm-transcript-retrieval` | search / cite / context / session retrieval ladders |
| `sm-persistence-taxonomy` | learn vs decide vs remember vs pin lane selection |
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

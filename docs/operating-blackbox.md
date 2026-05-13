# Operating blackbox - what landed in agentic-corpus, how to keep it healthy

This doc covers everything the bbox daemon now does beyond what the README
describes: the agentic graph surface, hybrid retrieval, the embedding
pipeline, and the upkeep steps that keep all of it healthy. Read in order
or jump via the section index. Cross-linked from the [README](index.md).

## Section index

- [What's new at a glance](#whats-new-at-a-glance)
- [The agentic graph surface](#the-agentic-graph-surface)
- [Hybrid search internals](#hybrid-search-internals)
- [Embedding pipeline](#embedding-pipeline)
- [Schema migrations](#schema-migrations)
- [Upkeep checklist](#upkeep-checklist)
- [Key locations on disk](#key-locations-on-disk)
- [System memories - on-demand runbooks](#system-memories-on-demand-runbooks)
- [Provider integration matrix](#provider-integration-matrix)
- [Open follow-ups](#open-follow-ups)

## What's new at a glance

The agentic-corpus arc added a graph projection layer over the same
transcript / knowledge / commit / file substrate that bbox already
indexed. Every entity has a canonical `<type>:<segments>` ref; entities
are connected by typed edges (CONTAINS_SYMBOL, CALLS, EDITED_BY_SESSION,
COMMIT_TOUCHED_FILE, KNOWLEDGE_FROM_SESSION, READ_FILE, etc); five new
MCP tools (`bbox_describe_schema`, `bbox_hybrid_search`,
`bbox_discover_seed_entities`, `bbox_inspect_entity`, `bbox_find_paths`,
`bbox_bundle_evidence`) compose into a 5-step opening sequence that
replaces the old "single bbox_knowledge call" cold-start pattern.

The hybrid retrieval blends BM25, vector cosine (Voyage embeddings or
Ollama fallback), and a path-token boost. The fusion runs RRF, then a
per-file collapse + modal diversification pass before returning the
top-N. The recall improvement on cold-start questions matches what the
donor McpPoc spike measured (~97% vs ~23% with naive rerank), with a
local benchmark reaching 4 of top-5 ground truth on the erlang-test
"recombination" probe and surfacing the .ex code chunks alongside docs
on the "triad implementation" probe.

## The agentic graph surface

`bbox_describe_schema` is the canonical orientation call. As of this
writing the corpus knows:

| Entity type | Population | Used for |
|---|---|---|
| `knowledge` | rules / decisions / conventions | "what's the policy on X?" |
| `project_file` | source / doc chunks (10454+) | "where does X live?" |
| `transcript` | one block of one Claude/Codex/Gemini session (~1M) | "what did this turn say?" |
| `session` | a full agent conversation (~4500) | "what was that session about?" |
| `thread` | persistent investigation across sessions | active arcs / deferred work |
| `note` | side-channel records (~6700) | dispute/done/blocked/etc |
| `symbol` | named code symbols (~6300) | call graph + defining file |
| `brofile` | persona+model+lens triple | dispatch provenance |
| `whiteboard` | multi-agent deliberation surface | board state |
| `commit` | git commits with parent + touched-file edges (~770) | version history |
| `task` (virtual) | bro_exec dispatch unit | what produced this artifact |
| `bash_call` (virtual) | one shell invocation in a transcript | what did this command emit |

Edge families (full list via `bbox_describe_schema`):

- **Structural** - IN_FILE, IN_SESSION, NEXT_SECTION, NEXT_CHUNK,
  PREV_CHUNK, THREAD_HAS_SESSION
- **AST** - DEFINED_IN, CONTAINS_SYMBOL, CALLS, USES_TYPE, HAS_FIELD,
  IMPLEMENTS_TRAIT
- **Knowledge** - SUPERSEDES, DERIVED_FROM, Contradicts,
  KNOWLEDGE_FROM_SESSION, KNOWLEDGE_FROM_BOARD
- **Provenance** - SESSION_USED_BROFILE, ARC_USED_BROFILE,
  ARC_OPENED_BOARD, NOTE_FROM_SESSION, NOTE_IN_THREAD, NOTE_FROM_TASK,
  TASK_PRODUCED_NOTE
- **Git** - COMMIT_PARENT, COMMIT_TOUCHED_FILE, COMMIT_PRODUCED_BY_ARC
- **Format-specific** - LINKS_TO_FILE, LINKS_TO_SECTION, DESCRIBES,
  ON_PAGE, FIGURE_OF, TABLE_OF
- **Tool-call** - EDITED_FILE, EDITED_BY_SESSION, READ_FILE, RAN_BASH

The 5-step opening sequence is documented in detail in the
`sm-agentic-opening-sequence` system memory. Pull on demand with
`bbox_knowledge(query="sm-agentic-opening-sequence")`.

## Hybrid search internals

`bbox_hybrid_search` fuses three ranked lists via RRF:

1. **bm25** - chunk-level Tantivy BM25 over `content`, `project`,
   `code_content`, `symbol`, `commit_author_name`, `path_tokens` fields.
   Path tokens use the code tokenizer (splits on `/_-.:>` plus
   CamelCase) and carry a 1.5× field boost. Symbol field also 1.5×.
2. **bm25_file** - sum-of-scores aggregated per `(project_id,
   rel_path_hash)`, then weighted by `sum * sqrt(chunk_count)`. Lifts
   high-coverage files (e.g. STATUS.md with 21 sparse mentions) that
   would otherwise be invisible to per-chunk ranking.
3. **vector** - per-route HNSW search via Voyage `voyage-code-3` (1024d)
   embeddings. RRF default weight 0.6 (0.4 BM25, 0.6 vector); set
   `vector_weight=0` for BM25-only, `1.0` for vector-only.

After fusion, the result list is post-processed:

- **Project filter** - when `project=<path or project_id>` is passed,
  drop project_file refs from other projects. Commits / knowledge /
  transcripts pass through. Cuts cross-repo keyword pollution that
  otherwise dominates ("voyage" returning erlang-test/voyage.ex above
  transcript-search/src/embed/voyage.rs).
- **Per-file collapse** - only the highest-scoring chunk per file
  survives. Mirrors the AgenticTools donor's diversity-by-file pass.
- **Modal diversification** - guarantees at least one
  `code_block` / `doc_section` / `git_message` in top-N when the fetch
  set has them. Stops a query like "triad implementation" from being
  10 docs with the defining .ex file invisible.
- **Symbol_exact boost** - single-token queries (snake_case, CamelCase,
  dotted) add a SHOULD clause matching `symbol_exact` with 6× boost so
  the defining chunk lifts above docs that just mention the symbol.

`bbox_discover_seed_entities` is the same call with `notable_edges`
rendered for each result - useful when the next step is
`bbox_inspect_entity` and you want pre-vetted hops.

## Embedding pipeline

Voyage embeddings drive the vector lane. The daemon runs a per-route
async queue (one worker per route) with debounce, batching, and retry.
Routes:

| Route | What's embedded | Default provider |
|---|---|---|
| code | source-file code chunks | voyage / voyage-code-3 |
| docs | source-file doc chunks (markdown) | voyage / voyage-code-3 |
| git_message | commit subject + body | voyage / voyage-code-3 |
| knowledge | knowledge-store entries | voyage / voyage-code-3 |
| notes | side-channel notes | voyage / voyage-code-3 |
| transcripts | transcript event blocks | voyage / voyage-code-3 |

Provider config lives in `src/embed/mod.rs::EmbeddingRouter`. Override
per-bucket or per-project in `~/.config/blackbox/embed.toml`.

### API key configuration

Production daemon needs `DAYSTROM_VOYAGE_API_KEY` (or `VOYAGE_API_KEY`)
in its environment. The systemd drop-in pattern:

```ini
# ~/.config/systemd/user/blackbox.service.d/voyage-key.conf
[Service]
Environment=DAYSTROM_VOYAGE_API_KEY=pa-...
```

Then `systemctl --user daemon-reload && systemctl --user restart blackbox.service`.

Falling back to `Ollama` works without an API key - set the route to
`ollama` in `embed.toml` to use a local `nomic-embed-text` (768d)
endpoint. Mixing Voyage and Ollama is fine; each route persists its own
HNSW partition keyed on `(provider, model, dimensions)`.

### Batch caps

The queue caps at **64 documents** or **80KB total** per Voyage request
(under the 128/120k Voyage limits). Without this cap a single restart
that re-fills the queue with thousands of pending chunks would send one
oversized request, get rejected, retry 3×, then DROP the entire batch.
See the cap definition in `src/embed/queue.rs::collect_quiescent_batch`.

### Status check

```bash
# Via MCP
bbox_embed_status()

# Or curl directly
curl -sN -X POST http://127.0.0.1:7264/mcp \
  -H 'content-type: application/json' \
  -H 'accept: application/json,text/event-stream' \
  -H "mcp-session-id: $SESSION" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"bbox_embed_status","arguments":{}}}'
```

Per-route fields: `available`, `provider`, `model`, `dim`,
`indexed_count`, `queue_depth`, `retried_count`, `last_error`. A healthy
daemon shows `available: true` everywhere with `last_error: null`.

## Schema migrations

Tantivy index schema changes are versioned via the
`INDEX_SCHEMA_VERSION` constant in `src/index/mod.rs`. On daemon start
the schema marker is compared and the index dropped + rebuilt if it
mismatches. A full rebuild on a 1.1M-doc corpus takes 5-7 minutes
including the EdgeIndex projection pass (which auto-fires via the
`blackbox-edge-rebuild` watcher thread when tantivy's doc count grows).

Recent schema bumps in chronological order:

- `agentic-corpus-g1` - initial agentic schema
- `agentic-corpus-g2-path-tokens` - added tokenized `path_tokens` field
- `agentic-corpus-g3-commit-subject-tokens` - populates `path_tokens`
  from commit subject too so commits compete on equal footing with
  project_files for ranking
- `agentic-corpus-g4-elixir-symbols` - elixir symbol extraction (defmodule/
  def/defp/defmacro/etc via tree-sitter `call` node filtering)
- `agentic-corpus-g5-symbol-tokenized` - `symbol` field switched from
  default tokenizer to code_tokenizer so `Substrate.TriadClosure`
  indexes as the union of [Substrate, TriadClosure, Triad, Closure]
  and matches both camelCase and snake_case query forms

Bumping the version triggers a full reindex; old segments are removed.

## Upkeep checklist

Daily / on-demand:
- `bbox_inbox(project="...")` - round-boundary attention sweep
- `bbox_thread_list(status="open")` - investigation continuity check

Per-release / when changing schema or chunker:
- Bump `INDEX_SCHEMA_VERSION`
- `cargo build --release && install -m 755 target/release/blackboxd ~/.local/bin/blackboxd && systemctl --user restart blackbox.service`
- Wait for `auto-reindex: indexed N files (M docs)` log line
  (~5-7 min for 1.1M docs)
- Wait for `edge-index watcher: corpus grew, EdgeIndex rebuilt` log line
  (~6 sec after reindex completes)
- Smoke: `bbox_describe_schema` should return all entity types with
  populations; `bbox_hybrid_search("test query", limit=5)` should return
  results with both `bm25` and `vector` sources contributing.

After tantivy schema bump:
- The schema-version marker file at `~/.local/share/blackbox/index/schema_version.txt`
  is rewritten on successful start. If you see "dropping transcript
  index for schema migration" in the journal, that's the expected path.

After embedding provider change:
- `bbox_reembed(route="<route>")` to re-fill the queue from existing
  indexed entities (lands in E3 and verified working)
- Watch `bbox_embed_status` for `queue_depth` to drain

After registering a new project:
- `bbox_project_register(path="/abs/path")` adds to the registry,
  triggers an EdgeIndex rebuild, and runs an incremental reindex
- The auto-reindex thread (120s tick) picks up new project files within
  the next 1-2 cycles
- For large repos (10k+ files) this can take 10+ minutes

## Key locations on disk

| Path | Contents |
|---|---|
| `~/.local/bin/blackboxd` | Production daemon binary |
| `~/.local/bin/blackboxd-dev` | Dev daemon binary (separate inode) |
| `~/.local/bin/bro` | Terminal TUI client |
| `~/.config/systemd/user/blackbox.service` | Prod systemd unit |
| `~/.config/systemd/user/blackbox.service.d/*.conf` | Drop-in env (e.g. voyage-key.conf) |
| `~/.local/share/blackbox/index/` | Tantivy index + schema_version.txt |
| `~/.local/state/blackbox/` | Knowledge / threads / notes / pins / projects / packets / artifacts JSON stores |
| `~/.local/state/blackbox/edges/<project_id>.jsonl` | Per-project edge sidecars |
| `~/.local/state/blackbox/vectors/` | HNSW partitions (one per provider+model+dim) |
| `~/.local/state/blackbox/backups/<ISO-ts>/` | Pre-render snapshots of provider markdown files |
| `~/.local/state/blackbox/git_meta/<project_id>.json` | Git history fingerprints for incremental indexing |
| `~/.bro/mcp.json` | Global MCP server config (per-provider sync) |
| `<project>/.bro/mcp.json` | Project-overlay MCP config |
| `~/.claude-shared/BLACKBOX.md` | Rendered tool reference + CORE RULEs (single source of truth, included by other provider files) |
| `~/.claude-shared/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md` | Per-provider memory files that include BLACKBOX.md by reference |

## System memories - on-demand runbooks

Code-embedded markdown runbooks queryable via `bbox_knowledge(query="sm-...")`.
Not rendered into provider files (kept cold; pulled when an agent reaches
for a primitive it hasn't used before).

| ID | Topic |
|---|---|
| `sm-agentic-opening-sequence` | The 5-step grounding pattern (orient → search → inspect → traverse → answer) |
| `sm-rule-packets` | Compile reusable judges/rubrics from examples |
| `sm-workflow-orchestration` | JSON workflow specs with per-node next transitions |
| `sm-bro-dispatch-patterns` | bro_exec / bro_resume usage |
| `sm-whiteboards` | Multi-agent deliberation boards |
| `sm-transcript-retrieval` | search / cite / context / session ladders |
| `sm-persistence-taxonomy` | learn vs decide vs remember vs pin lane selection |
| `sm-render-lifecycle` | Render → review → revoke flow |
| `sm-scoped-pins` | Active-arc context pins |
| `sm-create-etiquette` | List-before-create dedupe hygiene |
| `sm-side-channel-notes` | dispute/assumption/surprise/followup/blocked/learned/done |
| `sm-design-packets` | Multi-domain rule-packet design |
| `sm-auth-packets` | Authorization/policy packets |
| `sm-review-packets` | Review-style packets |

## Provider integration matrix

Validated grounding behavior across CLI providers when given a cold-start
question:

| Provider | Honors AGENTS.md @-imports | Reaches for bbox_* tools | Notes |
|---|---|---|---|
| `claude` (claude-opus-4-7) | ✅ | ✅ first-class | Followed full 5-step loop, caught cross-repo collisions naturally. Best cold-start reliability. |
| `codex` (gpt-5.5) | ✅ | ✅ first-class | Quality high, latency tends 2× of claude - verbose exploratory turns |
| `gemini` (gemini-3.1-pro) | ✅ | Untested in probe series | Renders to GEMINI.md; should mirror claude pattern |
| `glm` (zai-coding-plan/glm-5.1, via opencode) | ❌ | Falls back to grep/read | Opencode SQLite contention with parallel deepseek runs |
| `deepseek` (deepseek-v4-pro, via opencode) | ❌ | Falls back to grep/read | Same opencode integration gap |

Opencode-based bros currently grep instead of using the agentic surface
because opencode doesn't follow `@/path/...` includes in AGENTS.md by
default. Workaround under investigation; tracked in deferred-items
thread.

## Open follow-ups

Tracked in bbox threads; surface with `bbox_thread_list(status="open")`.

- `thread-3cfbf9e0` - agentic-corpus impl: deferred items across all phases
- `thread-f4e4624f` - Agentic-loop tools as composable subagents (scout
  brofile + workflow actor type)
- `thread-cba8bfa1` - workflow engine: foreach/matrix primitive
- `thread-e8f0371a` - apply_brofile_lens runs every turn - strip from
  resume paths
- `thread-7e2bd735` - workflow engine phase-next: template library + resume

Smaller items still open at session end:
- file: virtual entity refactor (currently chunk[0] proxies as the file)
- opencode bros not loading rendered guidance
- bbox_blame anchor-metadata file_path mismatch when multi-file edits
  share a commit
- Probe-team Q2-Q8 (only Q1 captured in this session)

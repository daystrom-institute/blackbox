# Agentic corpus — search, provenance, and producer machinery for blackbox

## 1. Thesis

Bbox already records every transcript turn, every tool call, every workflow node,
every whiteboard post, every knowledge entry written by every agent on the
machine, plus the git history of every project it tracks. The corpus is being
built every time anyone uses an agent — no separate ingestion is required to
populate the substrate.

The work is exposing that substrate as a **navigable graph** the calling LLM can
traverse. Provenance edges fall out of data the daemon already records. The
agentic search surface lets the LLM walk the graph. Producer-side machinery
(workflows + rule-packets + whiteboards) keeps the graph honest as the corpus
grows and contradicts itself.

This is **daystrom-lite**: the same epistemological-compounding ambitions as
daystrom-mk2, but projected over substrate the daemon already owns. No
closed-world system. No parallel database. No workflow enforcement. No editor
or dispatcher replacement. The user keeps using Claude Code / Codex / their
IDE / `bro_exec` unchanged; bbox adds the lens.

The opposite framing — vanilla RAG re-discovering knowledge from scratch on
every query, with the LLM authoring a wiki it then re-reads — is a strictly
weaker compounding story because it loses provenance. With provenance
first-class, every artifact is traceable to the conversation, persona, arc,
trigger, and commit that produced it. Pollution becomes traceable, not silent.
Causality is queryable, not inferred.

## 2. Two-concern architecture

The plan splits along this seam.

| Concern | Lives in | Drives |
|---|---|---|
| **Search surface** (consumer-facing) | Rust code in `src/` + MCP tool registry | What an LLM calling bbox sees |
| **Producer machinery** (ingestion + curation) | JSON workflows + rule-packets + whiteboards (user-installed; bbox ships examples) | What ends up in the corpus, how, when, with what gate |

The store is the meeting point. **No bbox-side LLM appears in any synchronous
search path.** The calling LLM is the runner. The agentic tools' descriptions
cue usage; nothing wraps them.

### 2.1 IaC pattern for producer machinery

Bbox ships **primitives** (workflow engine, rule-packet primitives,
whiteboard machinery, MCP tool registry, hook ops) plus **example
workflows / packets / brofiles** under `examples/agentic-corpus/`. Users
copy or adapt the examples and store the result in their own repos
(`<project>/.bbox/`) — the producer-side artifacts are infrastructure-as-code
held alongside the project they apply to. Cross-machine reproducibility lives
in git, not in `~/.local/state/blackbox/`.

This pattern follows `examples/keystone`, `examples/whiteboard`, and
`examples/sastquatch`. None of the producer arcs in this design fire by
default — the user installs whichever ones they want. If contradiction-review
isn't installed, tier-0 cosine detection emits a `bbox_note(kind=surprise)`
and stops; the calling LLM (or operator) can act on it manually.

## 3. Sources

- **Karpathy LLM-Wiki gist** — three-layer framing (raw / wiki / schema).
  Useful as orientation.
- **daystrom-mk2 / `spikes/Daystrom.Spike.McpPoc/AgenticTools.cs`** + design
  doc `design/agentic-discovery-tools.md`. The 30-query spike eval found
  agentic navigation tools beat static rerank pipelines 97% to 23%. The tool
  shape, descriptions, hint mechanics, and behavioral nudges port directly.
  Daystrom-specific epistemic vocabulary (Considerations / Decisions /
  Inquiries / CONSTRAINS / UPHOLDS) does not — substitute bbox's actual
  entity types.
- **daystrom-mk2 / `design/{semantic-engine,recall-and-contradiction,entity_search}.md`** —
  RRF hybrid formula, contradiction-detection tiers, canonical-entity-id
  fusion bug. Lift the formulas; skip the lens-scoped retrieval machinery.
- **daystrom-mk2 / `spikes/run-agentic-eval.sh`** — shell-script harness
  pattern. Adapt as the eval-matrix runner (avoids needing a workflow
  engine `foreach` primitive in v1).
- **erlang-test / `apps/substrate/native/substrate_native/src/{hnsw,fts,db,distance}.rs`** —
  per-kind tantivy + custom HNSW (1233 LoC), SIMD cosine via `wide::f32x8`,
  `m=32 / ef_construction=200 / ef_search=200`. Rust storage donor.
- **bbox engine itself** — `examples/keystone`, `examples/whiteboard`,
  `examples/sastquatch`. Reference patterns for producer-side machinery.

## 4. Search surface

### 4.1 Tool inventory

Eight new MCP tools plus one preserved.

| Tool | Purpose |
|---|---|
| `bbox_search` | Existing transcript-only BM25; preserved unchanged for backward compat |
| `bbox_hybrid_search` | Tantivy + HNSW + RRF over the full corpus; returns ranked typed entity refs |
| `bbox_discover_seed_entities` | Hybrid + type-aware ranking + `notable_edges` previews; entry point for the agentic loop |
| `bbox_inspect_entity` | Properties + filtered directed edges + recommended-next-hops + edge-family coverage |
| `bbox_find_paths` | Direction-preserving BFS, session-scoped monotonic path IDs (`P1`, `P2`, …) |
| `bbox_bundle_evidence` | Caller's close-the-loop: `(question, entity_refs, path_ids)` → packaged answer artifact |
| `bbox_describe_schema` | Catalog of entity types, properties, edge participation, filterable fields, edge family vocabulary with traversal tips |
| `bbox_embed_status` | Vector subsystem observability per route (§5.4) |
| `bbox_blame` | Walk `EDITED_BY_*` + git-blame from a `(file, line)` to the conversation + commit that produced the line |

`bbox_describe_schema` subsumes the daystrom spike's `list_edge_types` (one
endpoint, two response sections: vertex-type catalog + edge-type vocabulary).

`bbox_discover_seed_entities` and `bbox_hybrid_search` are peers. The first
conditionally cues agentic-tool-shaped follow-up via response shape (notable
edges previews on top results, hint text); the second is the bare ranked list.
The same hybrid composition runs underneath both. Tool descriptions tell the
LLM which to reach for.

### 4.2 Behavioral cuing — the description layer

Each tool's MCP `description` carries:
- One-sentence purpose
- 1-3 behavioral nudges (when to prefer, when to filter, what to compose with next)
- Anti-pattern warning where the failure mode is predictable

This is how we make search "smarter" without putting an LLM in the path. If the
descriptions aren't enough, the answer is sharper descriptions, not a wrapper.

Concrete example for `bbox_inspect_entity`:

```
Inspect a vertex: returns properties AND targeted edges in one call.
Prefer targeted inspection over broad sweeps:
  1) Set edge_types to the specific edges you want (e.g. 'SUPERSEDES,DERIVED_FROM').
  2) Set direction to 'out' or 'in' when you know which way to traverse.
  3) Use 'both' only for initial orientation on an unfamiliar entity.
  4) Set per_type_limit=0 for property-only inspection (no edges).

Do not answer governance, lifecycle, replacement, or history questions
from a single inspect call when the claim depends on a multi-hop chain;
validate the chain with bbox_find_paths first.
```

Across the agentic surface, these descriptions cue the LLM into the loop:
seed → inspect → traverse → bundle. The calling LLM is the runner; the
descriptions are how it learns the protocol.

### 4.3 No bbox-side LLM in the synchronous path

A load-bearing constraint:
- No "agentic runner workflow" wrapping the search tools.
- No per-call query-shape classification — neither LLM nor packet.
- No automatic answer synthesis — `bbox_bundle_evidence` is a packaging tool
  the LLM controls, not an LLM call.

All LLM judgment lives **producer-side**, in batch workflows, never on the
synchronous read path.

### 4.4 Error semantics

Every search-surface tool returns one of four response shapes — predictable
enough that the calling LLM can plan around them.

| Outcome | Status | Body shape |
|---|---|---|
| **Success** | `ok` | tool-specific result envelope |
| **Bad input** | `error.bad_input` | `{ code, message, field, suggested_fix }` — covers malformed entity-ref, invalid edge_type, out-of-range depth, etc. |
| **Not found** | `error.not_found` | `{ code, message, ref, similar_refs?: [...] }` — entity ref parsed but no matching entity (deleted, never existed, project not registered). `similar_refs` populated from EdgeIndex if the ref is close to a known entity. |
| **Partial / degraded** | `ok` with `degraded: { vector_status?, missing_providers?: [...], stale_path_ids?: [...] }` | embedding queue lagging, one provider down, path cache evicted entries the caller referenced. Result is best-effort; degradation is explicit. |
| **Server fault** | `error.server` | `{ code, message }` — internal panic, lock contention, etc. Caller retries. |

Specifics:
- Stale path IDs in `bbox_bundle_evidence` are reported per-id under
  `degraded.stale_path_ids`, not as a hard failure — the caller can re-run
  `bbox_find_paths` for missing IDs.
- Deleted chunks (content-hash superseded) return `not_found` with
  `similar_refs` populated to the new chunk-id at the same `(file, line range)`
  if EdgeIndex can resolve it.
- Provider timeouts on embedding never propagate; they back up the queue
  (§5.4). Search degrades to BM25-only and reports `vector_status.available=false`.

## 5. Storage substrate

### 5.1 Tantivy schema

Additive only — existing fields unchanged. Schema migration triggers a full
transcript reindex on first daemon start after upgrade (drop+rebuild; no data
loss because transcripts are immutable raw sources). The migration runs as a
workflow (§12.6) so search availability is observable.

```rust
// New FieldHandles
doc_type:        STRING | STORED,  // entity discriminator (see §6.1)
chunk_kind:      STRING | STORED,  // for project_file: code_block | doc_section | config_block | pdf_page | ...
language:        STRING | STORED,  // "rs" | "md" | "toml" | ...
symbol:          TEXT   | STORED,  // qualified name for code chunks (code-aware tokenizer)
symbol_exact:    STRING | STORED,  // exact symbol lookup
code_content:    TEXT   | STORED,  // code-aware tokenizer
chunk_hash:      STRING | STORED,  // SHA-256 hex; identity component of entity_id for project_file
entity_id:       STRING | STORED,  // canonical fusion key (§6.2)
parser_version:  STRING | STORED,  // bumped when transcript parser semantics change; triggers reindex
commit_sha:      STRING | STORED,  // for chunks observed at a known commit, for git_message: the commit
repo_id:         STRING | STORED,  // canonical hash of repo root (§5.6)
```

### 5.2 Tokenizers

Tantivy's default tokenizer breaks on `::`, camelCase, `>>`, snake_case stems.
Three text fields:

- `content` — default tokenizer. Prose, design docs, transcripts, knowledge
  entry bodies, commit messages.
- `code_content` — code-aware tokenizer. Splits on `_`, `::`, `.`, `>`,
  camelCase boundaries while keeping the original token. Code chunks emit
  into both `content` and `code_content`.
- `symbol_exact` — `STRING` field, no tokenization. Exact symbol lookup.

### 5.3 Vector store

Pattern from erlang-test's `substrate_native`, tightened with append-only WAL:

```text
~/.local/state/blackbox/vectors/
  meta.json       # dim, model, m, ef_*, parser_version, schema_version
  records.wal     # append-only log {entity_id, content_hash, model, dims, vector, deleted_at?, route}
  slab.bin        # contiguous f32 vectors, ordinal-indexed; rebuildable from WAL
  ids.bin         # ordinal → entity_id; rebuildable
  graph.bin       # HNSW neighbor lists; rebuildable
```

`records.wal` is the canonical store. `slab.bin` / `ids.bin` / `graph.bin` are
**rebuildable derived state** — startup validates against WAL watermark; rebuild
on mismatch.

HNSW parameters from erlang-test: `m=32`, `ef_construction=200`, `ef_search=200`.
SIMD cosine via `wide::f32x8`. Single-writer tokio task owns the slab; readers
take snapshots. Memory-pressure cliff is shared with erlang-test's design (slab
is `Vec<f32>`); revisit storage shape if vector count crosses ~1M.

If the user routes different buckets (§5.4) to providers with different dims,
the vector store is partitioned: one slab+graph per `(provider, dims)` tuple.
The `route` field on each record names which partition it belongs to. Search
queries the partition matching the bucket of the query (or all partitions if
unspecified, fusing per-partition ranks via RRF).

### 5.4 Embedding pipeline — config-driven per-bucket routing

Two providers behind one trait. **Routing is per-bucket and config-driven.**
The user picks per-bucket which provider embeds it.

```rust
trait EmbeddingProvider {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn model_name(&self) -> &str;
    fn id(&self) -> &str;  // "voyage" | "ollama"
}
```

Built-in providers:

| Provider | Default model | Dim | Auth | Use case |
|---|---|---|---|---|
| `voyage` | `voyage-code-3` | 1024 | env: `VOYAGE_API_KEY` | Code-aware, hosted |
| `ollama` | `nomic-embed-text` | 768 | none | Local, offline |

Buckets — a bucket is a logical group of content with shared embedding policy:

| Bucket | Default | Notes |
|---|---|---|
| `knowledge` | configurable | knowledge entry titles + bodies |
| `code` | configurable | code chunks (`code_content` field) |
| `docs` | configurable | markdown / plain text / config files |
| `transcripts` | configurable | transcript turns (user + assistant + tool results) |
| `git_message` | configurable | commit messages |
| `notes` | configurable | bbox_note bodies |

Configuration via `~/.config/blackbox/embed.toml`:

```toml
[embed.providers.voyage]
api_key_env = "VOYAGE_API_KEY"
model = "voyage-code-3"
rate_limit_per_min = 100

[embed.providers.ollama]
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
knowledge   = "voyage"   # the user wanting voyage everywhere just lists "voyage" everywhere
code        = "voyage"
docs        = "voyage"
transcripts = "voyage"
git_message = "voyage"
notes       = "voyage"

# A privacy-conscious user might write:
# knowledge = "ollama"; code = "ollama"; docs = "voyage"; transcripts = "ollama"; git_message = "voyage"; notes = "ollama"

# Per-project overrides (project_id is the realpath hash from §5.6):
[embed.routes.per_project."<project_id>"]
code = "ollama"   # this one project's code stays local
```

Embedding queue:
- Single tokio task, owns a `VecDeque<EmbedRequest>` per route.
- 5-second quiescence debounce per route. A bootstrap touching 200 chunks → one
  batch HTTP call per route.
- Content-hash skip via `chunk_hash` field; never re-embed unchanged content.
- Provider unavailable → that route's queue backs up, BM25 keeps working,
  hybrid search responses include `vector_status` keyed by route. **Never
  propagate embedding failures to search calls.**
- Rate limit per provider (config above).

`bbox_embed_status` returns per-route status:

```json
{
  "voyage": { "available": true, "indexed_count": 28341, "queue_depth": 12, "last_error": null },
  "ollama": { "available": true, "indexed_count": 0, "queue_depth": 0, "last_error": null }
}
```

### 5.5 EdgeIndex

In-memory normalized forward+reverse adjacency, rebuilt at daemon startup from
all stores. Forward and reverse traversal both cheap.

```rust
struct Edge {
    source: EntityRef,
    kind: EdgeKind,
    target: EntityRef,
    provenance: EdgeProvenance,    // explicit | derived | implicit
    confidence: EdgeConfidence,    // exact | heuristic | unknown
    metadata: EdgeMetadata,        // anchor (§14.2), edit_time, source_arc, etc.
}
```

**Edges that are projections** of existing fields are rebuilt from the source
store on startup (`SUPERSEDES` from `KnowledgeEntry.supersedes`, `IN_SESSION`
from tantivy `session_id`, etc.). The EdgeIndex is ephemeral for these.

**Edges that are authored** (Contradicts, RelatesTo, TensionWith, Supports,
DependsOn, semantic auto-edges from M5) live as durable fields on their
source entity:

```rust
// Extension to KnowledgeEntry — additive
pub struct KnowledgeEntry {
    // existing fields...
    #[serde(default)]
    pub links: Vec<KnowledgeEdge>,
}

pub struct KnowledgeEdge {
    pub target: String,           // entity ref string
    pub kind: EdgeKind,           // Contradicts | RelatesTo | TensionWith | Supports | DependsOn | DerivedFrom
    pub note: Option<String>,     // human-readable rationale
    pub source_arc: Option<String>,  // arc thread id, if produced by an arc
    pub confidence: EdgeConfidence,
}
```

EdgeIndex projects from `links` like everything else. The durable home for
authored edges is the source entity's JSON, not a new edge store.

For project_file chunks (which don't have a JSON-store home for edges —
they're tantivy docs), authored edges land in a sidecar
`~/.local/state/blackbox/edges/<project_id>.jsonl` rebuildable into the
EdgeIndex on startup. Tool-call provenance edges (§14) live there too.

Optional snapshot persistence at `~/.local/state/blackbox/edges/cache/`
keyed by generation; reload + delta-update on startup. Skip until rebuild
cost is measured.

### 5.6 Project + repo identity

`project_id` = realpath hash (sha256 of the canonicalized absolute path of the
project root, truncated to 8 hex). Stable across symlink aliases on the SAME
machine — the same underlying repo accessed from different paths collapses to
one project_id. **Per-machine by construction**; this is acceptable because
project_id is internal to one daemon's corpus.

`repo_id` is **content-derived** so it survives clones and is portable across
machines. Resolution order:
1. **First-commit SHA** — `git rev-list --max-parents=0 HEAD` returns the
   root commit. Hash truncated to 8 hex. Same for every clone of the same
   git history. Default for full clones.
2. **Remote URL** — `git config remote.origin.url`. Hash truncated to 8 hex.
   Fallback for shallow clones (which lack the root commit) and for cases
   where the first commit is rewritten by history-rewriting tools.
3. **Realpath hash** — same algorithm as `project_id`. Last-resort fallback
   for projects not under git, or git repos with no root commit and no
   remote (rare).

This matters for cross-machine portability of provenance (§15). When alice's
daemon writes `commit:<repo_id>:<sha>` into `refs/notes/bbox/*`, bob fetches
the notes; both daemons compute the same `repo_id` from the same git
content, so the edge target resolves consistently.

For projects not under git, `repo_id == project_id` and git-related
edges/entity types are absent.

There's no notion of "account" in entity refs. Bbox tracks one daemon, one
unified corpus, accessible from any client (Claude Code / Codex / Gemini /
direct MCP). The original multi-account framing was about Claude account
shells, not about the corpus structure; the corpus is account-agnostic.

### 5.7 Path cache

Per-MCP-session by default; configurable. Bounded LRU ~100 paths, evicts
oldest 30 on overflow. `bbox_find_paths` allocates monotonic IDs (`P1`, `P2`, …)
within a session; `bbox_bundle_evidence` reads non-consumingly. Process restart
drops the cache.

```toml
[paths]
cache_scope = "session"   # default; alternative: "process" (single-tenant convenience)
cache_size  = 100
```

### 5.8 vector_status response shape

Included in every `bbox_hybrid_search` and `bbox_discover_seed_entities` response:

```json
{
  "vector_status": {
    "voyage": { "available": true, "vectors_used_in_this_query": true,  "coverage_ratio": 0.97, "indexed_count": 28341, "queue_depth": 12 },
    "ollama": { "available": true, "vectors_used_in_this_query": false, "coverage_ratio": 0.00, "indexed_count": 0,     "queue_depth": 0  }
  }
}
```

`last_error` field per provider is sanitized — provider request details stripped.

## 6. Entity model

### 6.1 Entity types

Ten entity types plus two virtual.

| Entity type | Backing store | Notes |
|---|---|---|
| `knowledge` | existing `~/.claude-shared/blackbox-knowledge.json` | `bbox_learn` / `bbox_remember` / `bbox_decide` entries; new `links` field carries authored edges |
| `project_file` | tantivy index (chunks) + chunker registry | discriminated by `chunk_kind` (§7.2) |
| `transcript` | tantivy index (existing) | one doc per content block |
| `session` | derived from transcript scan | aggregated view of a session |
| `thread` | existing `threads.rs` | discriminated by `kind` (`investigation` \| `work_item`) |
| `note` | existing `notes.rs` | structured side-channel |
| `symbol` | tantivy index (code chunks) + AST walker | code-language symbols |
| `brofile` | existing `bro_brofile` store | persona templates |
| `whiteboard` | existing `whiteboards.rs` | deliberation lifecycle |
| `commit` | git via `gix` or shell-out (§9.6) | git commit; `commit_message` indexed in tantivy |

Virtual entities (no durable store; resolve through edges):

| Virtual entity | Resolution |
|---|---|
| `task` | resolves through producing session/thread |
| `bash_call` | resolves through `(session, turn)` to the originating transcript event |

Virtual entities are inspectable but **read-only and computed on demand** —
the `InspectableEntityProvider` for them resolves the virtual ref to its
materialized backing (a transcript event, a session) and returns a synthesized
view with the relevant edges. They appear in `bbox_describe_schema` as virtual
type rows.

### 6.2 Entity-ref grammar

Typed, parseable, stable across reindex. Single grammar entry point in Rust.

```text
knowledge:<entry_id>
transcript:<provider>:<session_id>:<line_offset>:<event_idx>
project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>
session:<provider>:<session_id>
thread:<thread_id>
note:<note_id>
symbol:<project_id>:<qualified_name>:<defn_hash>
brofile:<name>
whiteboard:<board_id>
commit:<repo_id>:<sha>
task:<task_id>             # virtual — resolves to owning session/thread
bash_call:<session>:<turn> # virtual — addressable bash event from tool-call provenance
```

Stability:
- `transcript:` IDs stable iff `parser_version` matches; bump → reindex.
- `project_file:` IDs use `chunk_hash` (content) + `occurrence_idx` (handles
  duplicate chunks within one file). Edits that don't touch a chunk's bytes
  leave its ID stable. Edits that do produce a new ID; old ID gets a tantivy
  `delete_term` and the vector slab marks the old ordinal as deleted.
- `symbol:` IDs use `defn_hash` (SHA-256 of the symbol's defining chunk text)
  so renames produce new IDs and old symbol entities tombstone.
- `commit:` IDs are git-stable forever.

### 6.3 EntityRef parser

```rust
enum EntityRef {
    Knowledge { id: String },
    Transcript { provider: String, session_id: String, line_offset: u64, event_idx: u32 },
    ProjectFile { project_id: String, rel_path_hash: String, chunk_hash: String, occurrence_idx: u32 },
    Session { provider: String, session_id: String },
    Thread { thread_id: String },
    Note { note_id: String },
    Symbol { project_id: String, qualified_name: String, defn_hash: String },
    Brofile { name: String },
    Whiteboard { board_id: String },
    Commit { repo_id: String, sha: String },
    Task { task_id: String },                  // virtual
    BashCall { session: String, turn: u32 },   // virtual
}

impl EntityRef {
    fn parse(s: &str) -> Result<Self> { ... }
    fn render(&self) -> String { ... }     // round-trips
    fn entity_type(&self) -> EntityType { ... }
    fn is_virtual(&self) -> bool { ... }
}
```

All MCP tools accept `EntityRef` strings as parameters; parse at the boundary;
return `error.bad_input` (§4.4) on parse failure with `suggested_fix` populated
when the malformed string is close to a valid grammar.

### 6.4 InspectableEntityProvider trait

Heterogeneous stores need a uniform inspection contract. Public facade
`bbox_inspect_entity` parses the entity-ref, dispatches to the registered
provider, renders one uniform response. Stores own facts; facade owns UX.

```rust
trait InspectableEntityProvider: Send + Sync {
    fn entity_type(&self) -> EntityType;
    fn owns_ref(&self, r: &EntityRef) -> bool;
    fn handles_virtual(&self) -> bool { false }   // virtual providers override

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView>;
    fn schema(&self) -> EntitySchemaView;
    fn forward_edges(&self, r: &EntityRef) -> Vec<Edge>;
    fn expected_edge_families(&self, r: &EntityRef) -> Vec<EdgeFamilyExpectation>;

    /// Type-aware "you probably want to inspect these" hints, computed
    /// from the FULL neighborhood regardless of caller's edge filter.
    /// Hardcoded per provider — not a packet.
    fn recommended_next_hops(
        &self,
        entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop>;

    /// 80-char truncation of the entity's name/title/text for inline display
    /// next to references. Critical UX feature from the daystrom spike.
    fn compact_label(&self, r: &EntityRef) -> Option<String>;
}
```

Per-provider `recommended_next_hops` sketches:
- `knowledge` → flag `SUPERSEDES`, `DERIVED_FROM`, `Contradicts`, `KNOWLEDGE_FROM_SESSION`, `KNOWLEDGE_FROM_BOARD`
- `project_file` (dispatching on `chunk_kind`):
  - `code_block` → `CALLS`/`CALLED_BY` (incoming + outgoing), `CONTAINS_SYMBOL` parent, `IN_FILE` parent, `EDITED_IN_COMMIT`
  - `doc_section` → `NEXT_SECTION`, `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `DESCRIBES`
  - `pdf_page` → `ON_PAGE` siblings, `FIGURE_OF`, `TABLE_OF`
- `transcript` → `IN_SESSION` parent, `EDITED_FILE`/`READ_FILE` (tool-call edges)
- `session` → `THREAD_HAS_SESSION`, `SESSION_USED_BROFILE`, child transcript events
- `thread` (dispatching on `kind`):
  - `work_item` → `ARC_USED_BROFILE`, `ARC_OPENED_BOARD`, `ARC_PRODUCED_COMMIT`, chronological note trail
  - `investigation` → member sessions, related notes
- `note` → `NOTE_FROM_TASK` source, `NOTE_IN_THREAD`
- `symbol` → incoming `CALLS` (callers), outgoing `CALLS` (callees), `DEFINED_IN`, `IMPLEMENTS_TRAIT`, `EDITED_IN_COMMIT` history
- `brofile` → recent `SESSION_USED_BROFILE`, `ARC_USED_BROFILE`, board agent registrations
- `whiteboard` → `BOARD_FROM_ARC`, `BOARD_REGISTERED_AGENT`, posts/votes inline
- `commit` → `COMMIT_PARENT`, `COMMIT_BY_AUTHOR`, `COMMIT_PRODUCED_BY_ARC`, files touched (`COMMIT_TOUCHED_FILE`)

## 7. Multimodal ingestion

### 7.1 Chunker registry

One trait, ordered first-claimer-wins.

```rust
trait SourceFormatChunker: Send + Sync {
    fn format_id(&self) -> &str;             // "code/rust", "markdown", "pdf", ...
    fn claims(&self, path: &Path, sniff: &[u8]) -> bool;
    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)>;
}
```

Registry holds an ordered `Vec<Box<dyn SourceFormatChunker>>`; `claims` is
first-match. Skip rules: gitignore, binary mime sniff, files >2MB,
`target/ node_modules/ _build/ .worktrees/`. Per-chunk size cap: 12KB.

### 7.2 chunk_kind discriminator

`project_file` chunks carry a `chunk_kind` field; the
`InspectableEntityProvider` for `project_file` dispatches on it.

| chunk_kind | Source format | Notes |
|---|---|---|
| `code_block` | tree-sitter language pack | function/struct/impl/trait boundaries; carries `symbol` |
| `doc_section` | markdown / asciidoc / rst | heading-split |
| `config_block` | toml/json/yaml | top-level key split |
| `paragraph` | plain text | paragraph boundaries |
| `pdf_page` | pdf | one chunk per page |
| `pdf_figure` | pdf | extracted figure caption + bbox |
| `pdf_table` | pdf | extracted table data |
| `spreadsheet_sheet` | xlsx/ods | one chunk per sheet (overview) |
| `spreadsheet_cell_range` | xlsx/ods | named range or formula-defined cluster |
| `notebook_cell` | ipynb | one chunk per cell, carries cell index + outputs |
| `slide` | pptx | one chunk per slide |
| `web_section` | html | heading-split or main-content extraction |
| `transcript_segment` | audio/video transcripts | time-segmented |
| `image_caption` | standalone image | VLM-generated caption |

### 7.3 Format coverage

| Format | Chunker | Edges added |
|---|---|---|
| Code (305 langs via tree-sitter-language-pack) | per-language tree-sitter walker (§8) | `CALLS`, `IMPLEMENTS_TRAIT`, `HAS_FIELD`, `CONTAINS_SYMBOL`, `USES_TYPE`, `IMPORTS`, `DEFINED_IN` |
| Markdown / asciidoc / rst | heading-split + link-extract | `NEXT_SECTION`, `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `EMBEDS_CODE_FENCE` |
| TOML / JSON / YAML | top-level-key split | `IN_CONFIG`, `KEY_REFERENCES` |
| Plain text | paragraph split | `NEXT_PARAGRAPH` |
| PDF | `pdf-extract` for text PDFs; `tesseract` shell-out for scans | `ON_PAGE`, `FIGURE_OF`, `TABLE_OF`, `CITATION_TO` |
| Excel / xlsx | `calamine` crate | `IN_SHEET`, `COMPUTED_FROM` (formula deps), `CELL_REFERENCES` |
| Jupyter notebooks (.ipynb) | cell-extract | `NEXT_CELL`, `OUTPUT_OF`, `IMPORTS_FROM_CELL` |
| DOCX / PPTX | `docx-rs` / `pptx` parser | `IN_SECTION`, `ON_SLIDE`, `IN_DECK` |
| HTML / web archives | `scraper` crate | `LINKS_TO_URL`, `EMBEDS_FRAME` |
| Audio/video transcripts | external (whisper) → time-segmented chunks | `AT_TIMESTAMP`, `IN_RECORDING` |
| Images standalone | VLM caption extraction | `DEPICTS`, `CAPTIONED_AS` |

### 7.4 Multimodal embeddings

Text-only embeddings cover all current chunkers via per-bucket routing (§5.4).
The chunker emits text into `code_content` / `content`; each bucket routes to
its configured provider. Visual embeddings (PDF figures, Excel charts, raw
images) become a third route option when a multimodal embedding model is
selected — see §20 open question.

## 8. Tree-sitter AST graph

### 8.1 Tier A — within-file + naive cross-file

`tree-sitter-language-pack` exposes both the chunker (`process()`) and the
parser (`get_parser(name)`); one parse pass feeds both. Walking the AST
during chunking yields deterministic structural edges:

| Edge | Source | Resolution |
|---|---|---|
| `DEFINED_IN` | symbol → file | trivial |
| `CONTAINS_SYMBOL` | file→symbol, struct→method, module→submodule | trivial — AST nesting |
| `HAS_FIELD` | struct → field | trivial |
| `IMPLEMENTS_TRAIT` | impl-block → trait-name | string-match |
| `CALLS` | function → callee-symbol-name | naive: project-wide symbol-table lookup by exact name |
| `USES_TYPE` | function/struct → type-name | same naive resolution |
| `IMPORTS` | file → import-target | text-level (`use foo::bar`, `from foo import bar`, etc.) |

Tier A is language-agnostic via the language pack and ships with code
chunking. Adds `symbol` entity type.

### 8.2 Tier B — full per-language name resolution

Correct cross-file `CALLS` / `USES_TYPE` requires real name resolution. Per
language, integrate via LSP-style query (rust-analyzer, pyright, omnisharp,
etc.) or compiler data. **Target language set:** rust, python, csharp, java,
go, typescript, javascript, c, cpp. Build-prop or per-language phase order;
implement opportunistically as the eval suite shows the gap.

### 8.3 Symbol entity

`symbol:<project_id>:<qualified_name>:<defn_hash>` — `defn_hash` ensures
renames produce new entities and old symbol IDs tombstone. `recommended_next_hops`:
- function → flag incoming `CALLS` (callers), outgoing `CALLS` (callees), `DEFINED_IN` (file), `EDITED_IN_COMMIT` history
- struct/class → flag incoming `IMPLEMENTS_TRAIT`, `CONTAINS_SYMBOL`/`HAS_FIELD` children, callers via methods
- trait/interface → flag incoming `IMPLEMENTS_TRAIT` (implementers)

## 9. Edge taxonomy

Edge kinds are open. `bbox_describe_schema` reflects what's actually populated.

### 9.1 Structural edges

`IN_FILE`, `NEXT_CHUNK`, `PREV_CHUNK`, `IN_PROJECT`, `IN_REPO`, `IN_SESSION`,
`IN_THREAD`, `IN_RECORDING`, `IN_SHEET`, `IN_DECK`, `IN_NOTEBOOK`, `IN_CONFIG`.

### 9.2 AST edges

See §8.1: `DEFINED_IN`, `CONTAINS_SYMBOL`, `HAS_FIELD`, `IMPLEMENTS_TRAIT`,
`CALLS`, `USES_TYPE`, `IMPORTS`.

### 9.3 Knowledge edges (authored — live in `KnowledgeEntry.links`)

`SUPERSEDES` / `SUPERSEDED_BY`, `DERIVED_FROM`, `Contradicts`, `Supports`,
`DependsOn`, `RelatesTo`, `TensionWith`. The first two are populated from the
existing `KnowledgeEntry.supersedes` field on read; the rest come from arc
verdicts (M4) or operator-authored writes. `DESCRIBES` and `REFERENCES` are
the v1 semantic auto-edges (§12.4).

### 9.4 Provenance edges (entity-level)

`SESSION_USED_BROFILE`, `THREAD_HAS_SESSION`, `TASK_PRODUCED_NOTE`,
`ARC_USED_BROFILE`, `ARC_OPENED_BOARD`, `ARC_FROM_TRIGGER`,
`ARC_PRODUCED_COMMIT`, `BOARD_REGISTERED_AGENT`, `BOARD_POST_BY_AGENT`,
`KNOWLEDGE_FROM_SESSION`, `KNOWLEDGE_FROM_TASK`, `KNOWLEDGE_FROM_BROFILE`,
`KNOWLEDGE_FROM_BOARD`, `KNOWLEDGE_FROM_ARC`.

### 9.5 Tool-call provenance edges (action-level)

Every tool call in a transcript is an indexable event. Tool calls project as
edges (see §14):

| Edge | Source | Anchor metadata |
|---|---|---|
| `EDITED_FILE` | tool-call(Edit/Write) → file at `(byte_range, content_hash, commit_sha?)` | full anchor (§14.2) |
| `READ_FILE` | tool-call(Read) → file at `(byte_range?, content_hash, commit_sha?)` | optional byte_range |
| `RAN_BASH` | tool-call(Bash) → `bash_call:<session>:<turn>` virtual | command + cwd + exit + stdout-summary in metadata |
| `EDITED_BY_SESSION` | reverse: file → session via anchor lookup | computed |
| `EDITED_BY_BROFILE` | reverse: file → brofile via session | computed |
| `EDITED_IN_ARC` | reverse: file → thread(kind=work_item) via session | computed |
| `EDITED_BY_TRIGGER` | reverse: file → arc → trigger event | computed |

### 9.6 Git edges (the corpus's deepest substrate)

Git is part of the graph. Every commit on every registered project's git
history is a `commit:<repo_id>:<sha>` entity; commit messages are indexed in
tantivy under `chunk_kind=git_message`.

| Edge | Source | Notes |
|---|---|---|
| `COMMIT_PARENT` | commit → commit (parent SHA(s)) | from `git log` |
| `COMMIT_BY_AUTHOR` | commit → author email/name (string) | git author trailer |
| `COMMIT_TOUCHED_FILE` | commit → project_file_chunk(s) | resolved at-commit; see §14.2 anchoring |
| `COMMIT_PRODUCED_BY_ARC` | commit → thread(kind=work_item) | populated when an arc's `on_arc_exit` records the commit SHA |
| `COMMIT_MENTIONS` | commit → entity refs found in commit message | `bbox-ref:` trailer or auto-detection |
| `IN_REPO` | commit → repo_id | implicit |

`bbox_blame` (§14.3) composes `EDITED_BY_*` + `COMMIT_TOUCHED_FILE` +
`COMMIT_PRODUCED_BY_ARC` to walk both bbox provenance and git history in one
chain.

### 9.7 Format-specific edges

Per chunker, see §7.3.

### 9.8 Edge confidence and provenance

Every edge carries `provenance ∈ {explicit, derived, implicit}` and
`confidence ∈ {exact, heuristic, unknown}`. Examples:
- `SUPERSEDES` from `KnowledgeEntry.supersedes` field → `(explicit, exact)`
- `IN_FILE` from `file_path` → `(implicit, exact)`
- `CALLS` from naive symbol-table → `(derived, heuristic)`
- `CALLS` from rust-analyzer Tier B → `(derived, exact)`
- `Contradicts` from contradiction-review verdict → `(explicit, heuristic)` with arc thread pointer
- `DESCRIBES` from auto-edge ensemble → `(derived, heuristic)` with vote tally

## 10. Ranking

### 10.1 RRF formula

Lifted from daystrom's `GraphSchemaService.HybridSearchClaimsAsync`, fixed for
the canonical-entity-id bug:

```rust
fn rrf_fuse(bm25: &[Hit], vec: &[Hit], w_vec: f32) -> Vec<Hit> {
    let mut merged: HashMap<String, f32> = HashMap::new();  // key = entity_id
    let w_bm25 = 1.0 - w_vec;
    let k = 60.0;
    for (rank, hit) in bm25.iter().enumerate() {
        *merged.entry(hit.entity_id.clone()).or_default()
            += w_bm25 / (rank as f32 + k);
    }
    for (rank, hit) in vec.iter().enumerate() {
        *merged.entry(hit.entity_id.clone()).or_default()
            += w_vec / (rank as f32 + k);
    }
    // sort desc, return top N
}
```

Default `vector_weight = 0.6`. The HashMap key is `entity_id`, not source-prefixed
strings. When a query crosses buckets routed to different providers (different
HNSW partitions), each partition produces its own ranked list and they're
fused with the BM25 list via the same formula.

### 10.2 Type-aware rerank multiplier

Applied only by `bbox_hybrid_search` and `bbox_discover_seed_entities`.
`bbox_inspect_entity` and friends never rerank.

| `doc_type` (+ subtype) | Multiplier |
|---|---|
| `knowledge` with `Approval::UserConfirmed` | ×1.35 |
| `knowledge` with `Approval::AgentInferred` | ×1.00 |
| `knowledge` with `Approval::Imported` | ×0.85 |
| `project_file` (chunk_kind=doc_section) | ×1.20 |
| `project_file` (chunk_kind=code_block) | ×1.00 |
| `project_file` (chunk_kind=git_message) | ×1.05 |
| `transcript` (role=user) | ×1.10 |
| `transcript` (role=assistant) | ×0.95 |

### 10.3 Temporal decay (knowledge only)

```text
score *= 1 + 0.3 * log2(1 + recall_count) * 2^(-days_since_recall / 21)
score *= 0.5 + 0.5 * 2^(-days_since_update / 30)   // recency floor of 0.5
```

### 10.4 No graph-proximity rerank by default

Daystrom's static rerank lost 23% to 97% in the spike eval. The agentic loop
is the graph-aware layer; the seed-finder is just for finding good seeds.

## 11. Producer machinery — overview

This is where workflows + rule-packets + whiteboards live. Strict producer-side;
nothing here runs synchronously inside a search call.

The pattern follows `examples/keystone`, `examples/whiteboard`,
`examples/sastquatch`. The engine knows nothing about RAG; everything
domain-specific is JSON. Per the IaC pattern (§2.1), bbox ships these arcs
as **examples** under `examples/agentic-corpus/`; the user installs them
into their own project's `.bbox/` to opt in.

## 12. Producer arcs

### 12.1 Bootstrap arc

```text
project-bootstrap-arc
  Setup            (hook-only)  — canonicalize project_id, init counters
  Walk             (hook-only)  — walkdir over project root, honor .gitignore, skip excluded
  Chunk            (hook-only)  — iterate chunker registry per file, emit chunks + format edges
  WriteIndex       (hook-only)  — tantivy add_document for each chunk
  EnqueueEmbed     (hook-only)  — enqueue chunks into per-route embedding queues
  DeriveEdges      (hook-only)  — emit IN_FILE / NEXT_CHUNK + AST edges into EdgeIndex
  IngestGitHistory (hook-only)  — walk git log; create commit entities; emit git edges (§9.6)
  Publish          (hook-only)  — bbox_note(kind=done) with chunk count, edge count, queue depth
  → terminal

policy_packet: workflow-policy/arc-budget
on_arc_exit:   workflow-cleanup/keep-on-fail
```

Triggers:
- Manual: `bbox_project_register(path=...)` MCP tool starts the arc once
- Cron: cron-inlet workflow re-runs incrementally
- Webhook (optional): "code pushed" event

### 12.2 Auto-digest arc

```text
auto-digest-arc
  Setup            (hook-only)  — read trigger event (task-completed signal payload)
  ProposeEntries   (executor)   — classifier-bro reads session transcript, emits candidate JSON entries
  Validate         (hook-only)  — parse_json, validate against entry schema, attach provenance
  QualityGate      (hook-only)  — packet: auto-digest/entry-quality
                                   lattice: [auto_apply | hold_for_review | reject]
  branch on verdict:
    auto_apply       → ApplyEntries (hook-only) → terminal
    hold_for_review  → SurfaceToInbox (hook-only) → terminal
    reject           → LogReject (hook-only) → terminal

policy_packet: workflow-policy/arc-budget
```

Constraints baked into the gate packet:
- `auto_apply` only for indexed-only, non-rendered, source-backed notes
  (`bbox_remember`-shaped). Rendered `bbox_learn` / `bbox_decide` always go to
  `hold_for_review`.
- Missing provenance → `reject` or `hold_for_review` per operator preference.
- Daily cap **configurable** via packet predicate; default high (e.g. 50/day)
  with a config knob.

Trigger: `task-completed` signal from `bro_exec` → routing-packet classifies
task_kind → `start_arc auto-digest-arc` for kinds where digest applies.

The provenance graph (§9.4 + §9.6) makes pollution traceable: every
auto-digested entry has explicit lineage `entry → KNOWLEDGE_FROM_TASK → task →
KNOWLEDGE_FROM_BROFILE → brofile → ARC_FROM_TRIGGER → trigger`. Bad entries
can be bulk-reverted by traversing reverse direction from any contaminating
brofile / arc / trigger.

### 12.3 Contradiction-review arc

```text
contradiction-review-arc
  Setup               (hook-only)  — fetch both entries, compute cosine, attach provenance
  OpenBoard           (hook-only)  — whiteboard_open + whiteboard_register × 4 (3 specialists + operator slot)
  BlindPost           (ensemble)   — specialists post stances per their lens
  TransitionToDebate  (hook-only)  — whiteboard_transition read → debate
  Debate              (ensemble durable) — same specialists annotate + vote
  TransitionToResolve (hook-only)  — whiteboard_transition resolve
  Synthesize          (facilitator) — emits structured JSON verdict
  ApplyVerdict        (hook-only)  — packet: contradiction/review-synthesis
                                     lattice: [contradicts | supersedes | tension_with | related | no_conflict | hold]
  branch on verdict:
    contradicts    → AppendKnowledgeLink(kind=Contradicts) → terminal
    supersedes     → AppendKnowledgeLink(kind=SUPERSEDES) → terminal
    tension_with   → AppendKnowledgeLink(kind=TensionWith) → terminal
    related        → AppendKnowledgeLink(kind=RelatesTo) → terminal
    no_conflict    → terminal
    hold           → SurfaceToInbox → terminal
  Done                (hook-only)  — whiteboard_archive
```

Edges write into `KnowledgeEntry.links` on the source entry (§5.5). EdgeIndex
projects on next read.

Three specialist brofiles:
- `contradiction-provenance` — source lineage, evidence quality, citation discipline
- `contradiction-lifecycle` — supersession, status, temporal scope
- `contradiction-coherence` — semantic compatibility, modal strength, scope

Operator joins as `agent_name=operator` on the same board. No special
escalation registry.

Trigger: synchronous embed-write detects cosine > 0.85 vs another entry not in
its supersession chain → emits `contradiction-detected` signal → routing-packet
→ start_arc. **If the user hasn't installed contradiction-review-arc, the
signal goes nowhere; the daemon emits a `bbox_note(kind=surprise)` instead and
keeps going.**

### 12.4 Auto-edge-extraction arc — `DESCRIBES` and `REFERENCES`

For semantic edges that need ensemble agreement:
- `DESCRIBES` — design-doc section ↔ code symbol ("this paragraph describes this struct")
- `REFERENCES` — knowledge entry ↔ code/file ("this entry references this implementation")

Structural code edges (CALLS, IMPLEMENTS_TRAIT) are deterministic AST
extraction (§8.1) and do NOT use this arc.

```text
auto-edge-arc
  Setup           (hook-only)  — load candidate (entity_a, entity_b, evidence_snippet)
  ClassifyVote    (ensemble)   — N classifier-bros vote independently per their lens
  Aggregate       (hook-only)  — packet: auto-edge/vote-aggregate
                                  lattice: [write_edge | hold_for_review | reject]
  branch:
    write_edge      → WriteEdge (to source entity's links field) → terminal
    hold_for_review → SurfaceToInbox → terminal
    reject          → terminal
```

Specialist brofiles per edge kind:
- For `DESCRIBES`: `describe-prose-signal`, `describe-symbol-fit`, `describe-narrative-cohesion`
- For `REFERENCES`: `reference-citation-precision`, `reference-target-existence`, `reference-context-fit`

Trigger options: scheduled scan over candidate pairs (markdown chunks ↔ symbols
in same project), or per-knowledge-entry scan on write.

### 12.5 Compaction arc

Cron-driven workflow. When deleted-ordinal ratio in any vector slab partition
exceeds threshold, rebuild that partition's HNSW from `records.wal`.

```text
embed-compaction-arc
  Setup       (hook-only)  — read vector_status per route, compute deleted_ratio per partition
  Decide      (hook-only)  — packet: embed/compaction-policy
                              lattice: [compact | notify | skip] (per partition)
  branch:
    compact → Quiesce → RebuildHnsw → SwapAtomic → terminal
    notify  → SurfaceToInbox → terminal
    skip    → terminal
```

### 12.6 Schema migration arc

```text
schema-migration-arc
  Setup       (hook-only)  — detect schema version mismatch in tantivy meta
  Quiesce     (hook-only)  — drain in-flight searches
  DropIndex   (hook-only)  — fs::remove_dir_all on tantivy path (transcripts immutable)
  Rebuild     (hook-only)  — re-walk transcript roots, re-emit docs (no LLM, no embedding)
  Verify      (hook-only)  — sample doc counts vs prior, sanity-check schema
  Swap        (hook-only)  — atomic rename; reopen reader; mark schema_version current
  → terminal
```

### 12.7 Eval arc (nightly)

```text
nightly-eval-arc
  Setup       (hook-only)  — invoke eval-matrix shell script (§16.4) to generate per-query workflow JSON
  RunSuite    (hook-only)  — shell-out: invoke eval harness against blackboxd-dev
  Score       (hook-only)  — parse harness JSON output, compare to baseline
  Decide      (hook-only)  — packet: eval/drift-policy
                              lattice: [stable | drift_minor | drift_major]
  branch:
    stable       → Publish (bbox_note + bbox_inbox digest) → terminal
    drift_minor  → Publish + AlertOperator → terminal
    drift_major  → OpenReviewBoard (whiteboard with eval-analysis specialists) → terminal
```

The shell-script approach (adapted from `daystrom-mk2/spikes/run-agentic-eval.sh`)
sidesteps the missing workflow-engine `foreach` primitive (tracked in
bbox_thread `thread-cba8bfa1`). When the engine grows `foreach`, this arc
collapses to native workflow.

## 13. Packet catalog

The minimum set; chunker-specific or arc-specific packets compose on top via
`Apply{packet_id, expect}`.

| Domain | Lattice | Use site |
|---|---|---|
| `workflow-policy/arc-budget` | `halt | warn | continue` | every workflow `policy_packet` |
| `workflow-cleanup/keep-on-fail` | `allow | deny` | cleanup hooks (reuse keystone shape) |
| `embed/compaction-policy` | `compact | notify | skip` | compaction arc decision |
| `auto-digest/entry-quality` | `auto_apply | hold_for_review | reject` | digest proposal gate |
| `auto-edge/vote-aggregate` | `write_edge | hold_for_review | reject` | semantic edge ensemble aggregation |
| `contradiction/review-synthesis` | `contradicts | supersedes | tension_with | related | no_conflict | hold` | contradiction whiteboard facilitator gate |
| `ingest/source-quality` | `accept | hold | reject` | per-file ingest gate (defer until ingest noisy) |
| `eval/drift-policy` | `stable | drift_minor | drift_major` | nightly eval verdict |
| `bro-trust/per-brofile` | `trusted | observe | quarantine` | composed by `auto-digest/entry-quality` to discount untrusted brofiles |

## 14. Tool-call provenance

### 14.1 What's already in the corpus

Every Claude Code / Codex / Gemini transcript records every `Edit`, `Write`,
`Read`, `Bash` as discrete tool-call blocks. The existing parser at
`src/parser.rs` handles tool_use/tool_result blocks — they're already indexed
as content blocks; they're just not exposed as edges.

### 14.2 Anchored edges (temporal correctness)

Tool-call edges DO NOT reference `chunk_entity_id` directly — chunk IDs are
content-hash-keyed and tombstone on edit. Instead, edges store an **anchor**:

```rust
struct ToolCallAnchor {
    file_path: String,           // relative to project root, canonical
    project_id: String,
    byte_range: Option<(u64, u64)>,    // for Edit/Write: the range modified; for Read: the range observed
    content_hash_at_edit: String,      // SHA-256 of the file at edit time
    commit_sha_at_edit: Option<String>,// if known (clean working tree at edit time)
    edit_timestamp: DateTime<Utc>,
}
```

The anchor is the durable record. `chunk_entity_id` is computed at query time
by:
1. Look up the file's current state.
2. If `content_hash_at_edit` matches current → resolve to current chunk_id at
   that byte_range.
3. If not, walk forward through git history using `commit_sha_at_edit` →
   `git blame` to find where the byte_range moved (if at all). If the line
   still exists in current HEAD, return the current chunk_id; otherwise,
   return a synthetic "historical" entity_ref the caller can inspect to see
   the prior state.

This makes `bbox_blame(file, line)` accurate across edits: the requested line
walks back through git blame to the originating commit, which matches against
the commit_sha in tool-call anchors, which yields the editing session.

### 14.3 bbox_blame

Convenience tool. Given `(file, line)` or `entity_ref` for a chunk:
1. Use git blame on the live file at the current HEAD to identify the
   commit that introduced the line.
2. Match against `commit_sha_at_edit` in tool-call anchors. If matched, walk
   `EDITED_BY_SESSION` → `IN_SESSION` → `SESSION_USED_BROFILE` /
   `THREAD_HAS_SESSION` → `ARC_USED_BROFILE` / `ARC_FROM_TRIGGER` and render
   the chain.
3. If no anchor match (commit was not produced through bbox-tracked tool
   calls), fall back to `git blame` author info only, marked `non-bbox`.

Includes the originating prompt context (read tool-call edges from the same
session preceding the edit), so the chain reads:

```
src/main.rs:42 was last edited by:
  commit a1b2c3d (2026-04-22 15:30 UTC)  by keystone-impl (Sonnet 4.6)
  in arc wf-implementer-feedback-arc-007 (PR #117)
  triggered by Forgejo review comment "scope check missing"
  informed by prior reads of:
    - src/auth.rs (this session, turn 4)
    - design/auth-jwt-conventions.md (this session, turn 3)
```

## 15. Git serialization for provenance

### 15.1 Approach: git notes

Bbox writes provenance summaries as git notes under
`refs/notes/bbox/provenance`. One note per commit, JSON body:

```json
{
  "commit": "a1b2c3d...",
  "produced_by": {
    "session_id": "claude-session-...",
    "brofile": "keystone-impl",
    "arc_thread_id": "thread-...",
    "trigger": { "kind": "forgejo_webhook", "ref": "pr#117#review" }
  },
  "tool_calls": [
    { "tool": "Edit", "file": "src/main.rs", "byte_range": [1024, 1156], "turn": 7 },
    { "tool": "Read", "file": "src/auth.rs", "turn": 4 },
    ...
  ],
  "knowledge_writes": [ { "id": "know-...", "kind": "auto_apply" } ]
}
```

Why git notes:
- Designed exactly for this purpose (out-of-tree commit metadata).
- Don't touch the working tree; no merge noise.
- `git fetch refs/notes/bbox/*` pulls them on clone; bbox bootstrap arc
  reads them and replays into the EdgeIndex.
- Same machinery extends to knowledge entries: `refs/notes/bbox/knowledge`
  carries decision/learn writes per commit.

### 15.2 Cross-machine sync

When bbox encounters a project where `refs/notes/bbox/*` exists, the
bootstrap arc (§12.1) reads the notes and replays edges + knowledge writes
into the EdgeIndex + knowledge store. New writes append new notes.

### 15.3 Conflict resolution

Git notes have a per-namespace merge driver. For bbox notes, the merge
strategy is **append**: both sides' tool_calls and knowledge_writes lists are
unioned by entry_id / turn. Manual operator review surfaces if the same
entity_id has divergent kinds in both sides.

### 15.4 Git as part of the graph

Beyond serialization, git itself becomes traversable:
- Every commit on every registered project is a `commit:<repo_id>:<sha>`
  entity (see §6.1, §6.2).
- Commit messages are indexed in tantivy under `chunk_kind=git_message` and
  searchable.
- `COMMIT_PARENT`, `COMMIT_BY_AUTHOR`, `COMMIT_TOUCHED_FILE`,
  `COMMIT_PRODUCED_BY_ARC`, `COMMIT_MENTIONS` edges (§9.6).
- `bbox_find_paths(from=knowledge:abc, edge_types="DERIVED_FROM,COMMIT_TOUCHED_FILE,COMMIT_PRODUCED_BY_ARC")`
  → "this knowledge derived from a commit produced by an arc."
- `bbox_inspect_entity(commit:repo123:a1b2c3d)` → message, parents, author,
  files touched, the arc that produced it (if known), the knowledge writes
  associated.

## 16. Eval surface

### 16.1 Query suite

5 query classes, sized to whatever supports a meaningful gate (typical: 6 per
class, 30 total):
1. **Exact symbol** — "what file defines `KnowledgeStore`?"
2. **Conceptual design-doc** — "how does the recursion guard work?"
3. **Stale-decision lookup** — "what was decided about Postgres consolidation?"
4. **Transcript provenance** — "where did we discuss Voyage embeddings?"
5. **Cross-modal code/prose** — "show me the design doc and the code that implements it"

Per-query manifest includes:
- query text
- expected entity refs (one or more)
- required edge family or path
- forbidden stale answers
- pass-classifier function name (Rust check_pass implementation)

### 16.2 Three-strategy comparison

Per query:
1. **Search-only baseline** — `bbox_search` only
2. **Static hybrid** — one `bbox_hybrid_search` call, no inspection loop
3. **Agentic** — full tool surface, calling LLM drives the loop

### 16.3 Gates

- Agentic must hit ≥ 27/30 overall.
- No class < 5/6.
- Agentic must beat search-only by ≥ 10pp.
- Static hybrid must beat search-only by ≥ 10pp to ship as a default.

### 16.4 Harness shape — shell script

Adapted from `daystrom-mk2/spikes/run-agentic-eval.sh`. Iterates the query
suite, dispatches a stock LLM with the bbox tools attached against
blackboxd-dev over HTTP MCP. Per-query JSON verdict; aggregate scoreboard.
Run nightly via cron-inlet workflow (§12.7).

The shell-script approach sidesteps the missing workflow-engine `foreach`
primitive (tracked in bbox_thread `thread-cba8bfa1`). The harness is a
script (not a workflow) because workflows are for multi-actor protocols, not
"loop over a list of test cases."

## 17. Observability

- `vector_status` per route in every hybrid_search response (§5.8).
- `bbox_embed_status` standalone tool: per-route queue depth, last error,
  indexed count, model.
- `bbox_inbox` surfaces:
  - knowledge entries in `hold_for_review`
  - open contradiction-review whiteboards waiting on synthesis
  - eval drift alerts
  - failed bro tasks
  - tier-0 contradictions when contradiction-review-arc isn't installed
- Every workflow arc opens a `bbox_thread(kind=work_item)` with per-node
  `done`/`learned`/`surprise`/`blocked` notes — full audit trail
  reconstructable from the thread alone via `bbox_notes(thread_id=...)`.

## 18. Boundaries — what daystrom-lite is NOT

- **Not a closed-world system.** Bbox observes; doesn't own dispatch,
  storage, or reasoning end-to-end.
- **Not a parallel database.** EdgeIndex is a projection over existing
  stores plus authored fields on those stores (e.g. `KnowledgeEntry.links`),
  not a new substrate.
- **Not a workflow enforcer.** Workflows are opt-in producer machinery.
  Users can keep using `bro_exec` directly, or no MCP at all; the corpus
  still grows from transcripts.
- **Not a replacement for the editor.** Claude Code, Codex CLI, IDEs keep
  working unchanged.
- **Not a replacement for git.** Git owns versioning, branching, merging,
  blame. Bbox adds the conversational layer (provenance + arcs + boards)
  that decorates git's content layer.
- **No bbox-side LLM in the synchronous search path.** The calling LLM is
  the runner.

## 19. Non-goals

- Auto-digest with `auto_apply` for rendered entries (only indexed-only).
- Lens-scoped retrieval as a first-class search parameter.
- Concept aliases / lifecycle vocabulary / convergence metric / per-edge
  graph weights — daystrom-grade refinements unjustified at bbox scale.
- Compiled lint packets — existing `bbox_lint` heuristics adequate.
- Auto-cosine provenance backfill on existing knowledge entries — forward-only.
- Per-call query-shape classification — overengineering.
- Hand-authored eval matrix workflows — generate from manifest via shell.
- Closing the daystrom epistemic vocabulary into bbox.
- Multi-account project-id distinction — accounts are orthogonal to the
  unified corpus; bbox tracks one daemon, one set of project_ids.

## 20. Open design questions

1. **Per-turn MCP tool-call budget.** `policy_packet` watches workflow steps,
   not LLM-internal tool calls. The producer arcs are fine; the agentic
   search surface has no per-turn budget today. Three options: accept soft
   prompt-only budget, limit per-tool via response-size cap, write a
   per-MCP-session tool-call counter in Rust. Decision pending eval signal.
2. **Workflow `foreach` engine primitive.** Tracked in bbox_thread
   `thread-cba8bfa1`. Eval matrix uses shell-script workaround for now;
   re-evaluate priority once a second use case surfaces.
3. **Multimodal embedding model selection.** When PDF figures, Excel charts,
   or standalone images become a measured eval gap, evaluate
   `voyage-multimodal-3` vs CLIP vs open alternatives. Until then, text-only
   embeddings cover all chunkers via the per-bucket route config.
4. **Per-language LSP integration for Tier B AST.** Target language set
   listed (§8.2). Order: rust-analyzer first; others opportunistically as
   eval shows the gap. Out-of-tree extension point.
5. **Whiteboard specialist auto-post.** Today specialists are
   prompt-instructed to call `whiteboard_post`. Silent failure if forgotten.
   Either add a completion-gate per node (parse the actor's output, retry if
   no post landed) or add engine-driven auto-post. Defer until
   contradiction-review shows the failure in practice.
6. **Vector partitioning for cross-route fusion.** When a query crosses
   buckets routed to different providers (and hence different HNSW
   partitions), the RRF fusion runs per-partition and merges. Edge case:
   what if the same entity_id is in two partitions (e.g. user re-routed a
   bucket and didn't compact)? Tentatively: collapse via entity_id with the
   higher partition's rank winning ties.
7. **Git-notes namespace conflict with other tools.** `refs/notes/bbox/*`
   could conflict with other note-using tools. Plan: namespace by daemon
   instance (e.g. `refs/notes/bbox-prod/`, `refs/notes/bbox-dev/`).
   Reconfigurable via `[git] notes_namespace = "..."`.

## 21. Glossary

- **Agentic surface** — the set of MCP tools the calling LLM uses to navigate
  the corpus. Pure code; no bbox-side LLM in the path.
- **Anchor (tool-call)** — `(file_path, byte_range, content_hash_at_edit, commit_sha_at_edit)`
  tuple stored on tool-call edges so they survive chunk supersession (§14.2).
- **Authored edge** — an edge whose source is an explicit decision (operator,
  arc verdict, semantic auto-edge ensemble) rather than a projection of
  existing structural data. Lives in `KnowledgeEntry.links` or sidecar
  edge files.
- **Bucket (embedding)** — a logical content group with shared embedding
  policy (`knowledge`, `code`, `docs`, `transcripts`, `git_message`, `notes`).
  Each bucket has a configured provider route (§5.4).
- **Calling LLM** — whichever LLM is invoking bbox tools (Claude Code, Codex,
  Gemini, the user's IDE assistant). The runner of the agentic loop.
- **Chunker registry** — first-claimer-wins ordered list of
  `SourceFormatChunker` impls; one per source format (markdown, code, PDF,
  etc.). The single place that knows the format (§7.1).
- **Classifier bro** — an executor with a narrow lens + structured JSON output
  (parsed via `on_exit parse_json`) + a downstream rule-packet that classifies
  the structured output. Used in producer arcs only, never on synchronous
  search path.
- **Daystrom-lite** — the framing for this work: epistemological-compounding
  ambitions of daystrom-mk2, projected over substrate bbox already records.
- **Edge family coverage** — `bbox_inspect_entity` response field that
  enumerates every edge family the entity type participates in, including
  ones with zero observed instances. Negative evidence: "checked, none
  found" vs "didn't query." Anti-hallucination signal for the calling LLM.
- **EdgeIndex** — in-memory normalized forward+reverse adjacency over all
  entity types. Projection of existing stores plus authored fields;
  ephemeral; rebuilt at startup.
- **Entity-ref** — typed string identifier for any entity in the corpus.
- **Producer arc** — a workflow that builds or curates the corpus.
- **Provenance edge** — an edge whose semantic content is "X produced Y" or
  "Y was authored under conditions X". Falls out of existing data; the work
  is exposing as edges, not collecting.
- **Recommended next hops** — per-entity-type list of edge families the
  calling LLM should likely follow, computed from the full neighborhood
  regardless of caller's edge filter. Hardcoded per provider (§6.4).
- **Route (embedding)** — the (provider, model, dim) tuple that a bucket
  embeds with. Each route has its own slab+graph partition in the vector
  store (§5.3).
- **Rule-packet** — a compiled rule set (`bbox_compile`) that classifies a
  structured entity into a lattice value via predicates. Deterministic, no
  LLM in the receive path. Used as gate packets, policy packets, routing
  packets, aggregation packets.
- **Seed** — the result of `bbox_discover_seed_entities` or
  `bbox_hybrid_search`: a starting entity for subsequent graph navigation.
  Not the answer.
- **Tier A AST** — within-file + naive cross-file resolution.
  Language-agnostic via tree-sitter. Deterministic.
- **Tier B AST** — full per-language name resolution via LSP-style integration.
- **Tool-call provenance** — projecting Edit/Write/Read/Bash tool calls in
  transcripts as edges between project_file chunks and the conversations
  that edited / read / executed against them.
- **Vector_status** — per-route observability struct included in every
  hybrid-search response (§5.8). Tells the caller which routes participated,
  which are degraded, and what queue depth is.
- **Virtual entity** — an entity that has no durable store; resolved on
  demand through edges to its materialized backing. v1 virtuals: `task`,
  `bash_call`.
- **Whiteboard** — engine-native deliberation primitive (`whiteboard_*`
  tools). Multi-agent posts/annotations/votes with phases (blind / read /
  debate / resolve / archive). Operator can join as another agent.
- **Workflow / arc** — engine-native multi-phase orchestration primitive
  (`bro_orchestrate_run`). JSON spec with nodes, gates, transitions. Every
  arc opens a `bbox_thread(kind=work_item)` for audit.

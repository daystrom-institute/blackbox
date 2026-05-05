# Agentic corpus — implementation skeleton

Companion to `design/agentic-corpus.md`. Each phase below names a discrete
implementation chunk: scope, components, gates (what proves it's done),
known follow-ups, and the design-doc sections it realizes. Phases are
dependency-ordered. No timelines — landing one phase unblocks dependents,
landing all phases realizes the design.

Phases tagged `[marker]` sit at known dependency positions but await detail
fleshing. Deliberation rounds will pick which markers to flesh out next.

---

## Foundation

### Phase F1 — Entity-ref grammar + parser

**Scope.** `EntityRef` enum + parser/renderer used at every MCP tool boundary.

**Realizes.** Design §6.2, §6.3.

**Components.**
- `src/entity_ref.rs` — `EntityRef` enum with all 12 variants (10 entity
  types + 2 virtual: `commit`, `task`, `bash_call`).
- `EntityRef::parse(&str)`, `EntityRef::render()`, with round-trip tests.
- `EntityType` enum + `EntityRef::entity_type()` + `EntityRef::is_virtual()`.
- `project_id` / `repo_id` canonicalization helpers (realpath hash, §5.6).
- `parser_version` constant declared (bumped at parser-semantic changes).

**Gates.**
- Round-trip property test passes 10k random entities.
- `commit:<repo>:<sha>` round-trips through realpath-hash repo_id correctly.
- `parse(error.bad_input)` returns the §4.4 shape with `suggested_fix` when
  the input is close to a valid grammar.

**Follow-ups.** None blocked; downstream phases consume the parser at boundaries.

---

### Phase F2a — Eval suite skeleton

**Scope.** Define the 30-query suite as data manifests + per-query check_pass
function names. **No gates that depend on later phases** — F2a establishes
the contract, F2b (later) flips on the resolved-entity-ref gate once
dependent phases land.

**Realizes.** Design §16.1.

**Components.**
- `eval/queries/` directory: one TOML/JSON file per query.
- Each manifest carries: query text, expected entity refs (target locators,
  resolved or unresolved), required edge family or path, forbidden stale
  answers, pass-classifier function name.
- `eval/check.rs` — per-query `check_pass(collected: &[EntityRef]) -> (bool, Vec<String>)`
  signature + a default implementation.
- Stub manifests for 6 × 5 = 30 queries with **target locators** (the human
  description of what the right answer points at) — not yet resolved entity
  refs.

**Gates.**
- All 30 manifests parse correctly.
- The `check.rs` skeleton compiles with stubbed pass functions.

**Follow-ups.** F2b lands the resolved-entity-ref gate. H3 consumes the
suite via shell harness.

---

### Phase F3 — Schema migration arc + tantivy schema additions

**Scope.** Land additive tantivy schema changes (§5.1) + schema-migration-arc
(§12.6). New fields populated by existing reindex flow; transcript reindex
runs on first daemon start after upgrade.

**Realizes.** Design §5.1, §5.2, §12.6.

**Components.**
- Extend `src/index/mod.rs` `FieldHandles` with new fields (incl.
  `commit_sha`, `repo_id` from §5.1 git-aware additions).
- `src/index/reindex.rs` populates new fields for transcripts.
- `examples/agentic-corpus/workflows/schema-migration-arc.json`.
- `examples/agentic-corpus/packets/workflow-policy/arc-budget.json` —
  required for every workflow.
- Document upgrade-time reindex pause in release notes.

**Gates.**
- Existing tests pass after schema changes.
- Fresh daemon start on existing transcript corpus rebuilds index cleanly;
  doc count matches prior generation.
- `bbox_search` continues to work transparently.

**Follow-ups.** S1 (project registration) starts populating
`doc_type=project_file`.

---

### Phase F4 — Artifact catalog + install/versioning (was H4)

**Scope.** Workflows + packets + brofiles need a shared catalog with
version + supersession tracking. Lands BEFORE the first artifact ships
(F3 already ships one) so examples have upgrade semantics from day one.

**Realizes.** Design §2.1 (IaC pattern), §11.

**Components.**
- `bbox_artifact_install` MCP tool: install a workflow / packet / brofile
  from a JSON file or remote URL.
- `bbox_artifact_list` MCP tool: list installed artifacts with version +
  source.
- `bbox_artifact_supersede` MCP tool: mark an artifact superseded by
  another (analogous to `bbox_decide` supersession).
- HTTP `/admin/artifact/*` endpoints for shell-script installers (the
  keystone pattern).
- Per-project install discovery: when bbox detects a `<project>/.bbox/`
  directory at registration time, auto-install (or prompt for) artifacts
  found within.
- Versioning: each artifact JSON carries `version` + optional `supersedes`
  field; daemon tracks the active version per name.

**Gates.**
- Install + supersede + list round-trip cleanly on a sample artifact.
- A user replacing the example `auto-digest-arc.json` with a customized
  version supersedes cleanly without daemon restart.
- The keystone install pattern (`scripts/install.sh`) ports to the new
  artifact endpoints without behavior change.

**Follow-ups.** Every later phase that ships a workflow/packet/brofile
uses this catalog.

---

## Storage substrate

### Phase S1 — Project registration

**Scope.** `bbox_project_register(path)` + persistent `ReindexConfig::project_roots`
list. No file indexing yet.

**Realizes.** Design §5.6.

**Components.**
- `bbox_project_register` MCP tool: canonicalize path → realpath hash →
  project_id; persist to `~/.local/state/blackbox/projects.json`.
- `bbox_project_list` MCP tool.
- `ReindexConfig` extended; reindex thread reads project list every cycle.
- `repo_id` derived for projects under git (`gix` discovery).
- Symlink alias collapse: same realpath → same project_id, no override
  flag needed (per-account distinction not a concern; §5.6).

**Gates.**
- Registering this repo + a sibling repo produces stable project_ids.
- Symlink to a registered project resolves to the same project_id.
- `repo_id` populated correctly for projects under git, absent otherwise.

**Follow-ups.** S2 starts indexing registered projects.

---

### Phase S2 — Project-file indexing (markdown + config + plain text) + bootstrap arc skeleton

**Scope.** Walk registered projects, chunk markdown / TOML / JSON / YAML /
plain text, write to tantivy. Code chunking deferred to S3. Embeddings
deferred to E1-E3 (queue stub no-ops). **Land bootstrap arc alongside this
phase** so the producer-side spine exists before later phases reach for it
(per codex #9).

**Realizes.** Design §7.1 (chunker registry), §7.2 (chunk_kind), §7.3 (text
formats only), §12.1 (bootstrap arc structure).

**Components.**
- `src/chunker/mod.rs` — `SourceFormatChunker` trait + registry.
- `src/chunker/markdown.rs` — heading-split + link-extract (`NEXT_SECTION`,
  `LINKS_TO_FILE`, `LINKS_TO_SECTION`, `EMBEDS_CODE_FENCE` edges).
- `src/chunker/config.rs` — TOML/JSON/YAML key-split.
- `src/chunker/text.rs` — paragraph split.
- Reindex integration: walk project root, dispatch each file through
  registry, write chunks with `chunk_kind` field.
- Skip rules: gitignore, binary mime sniff, size cap.
- `examples/agentic-corpus/workflows/project-bootstrap-arc.json` with
  `EnqueueEmbed` node hook stubbed (no-op until E2 lands).
- Manual trigger via `bbox_project_register` starts the arc.

**Gates.**
- Bootstrap this repo: arc completes; `bbox_search(query="agentic-corpus")`
  returns hits from `design/agentic-corpus.md`.
- `bbox_search(query="trait SourceFormatChunker")` returns the design doc
  (no code chunker yet).
- Re-running the arc on the same project is idempotent (content-hash dedup
  elides unchanged chunks).
- `bbox_arc_status` shows progress through nodes during a run.

**Follow-ups.** S3 adds code chunking. K1 adds knowledge entry indexing.
G1 adds git history. E1-E3 promote `EnqueueEmbed` from no-op to live.

---

### Phase S3 — Code chunking via tree-sitter-language-pack + Tier A AST edges

**Scope.** Add `tree-sitter-language-pack` dep with static-compiled subset
(`TSLP_LANGUAGES=rust,python,csharp,java,go,typescript,javascript,c,cpp`).
Code chunker emits chunks AND Tier A AST edges. Introduce `symbol` entity type.

**Realizes.** Design §7.3 (code formats), §8.1 (Tier A), §8.3 (symbol entity).

**Components.**
- Add `tree-sitter-language-pack` to Cargo.toml; disable `download` feature.
- `src/chunker/code.rs` — uses `process()` for chunks AND `get_parser(name)`
  for AST walking; one parse pass two consumers.
- AST walker emits: `DEFINED_IN`, `CONTAINS_SYMBOL`, `HAS_FIELD`,
  `IMPLEMENTS_TRAIT` (string-match), `CALLS` (naive symbol-table),
  `USES_TYPE` (naive), `IMPORTS` (text-level).
- Project-wide symbol table built during bootstrap; resolution happens
  after all files chunked (two-pass).
- `symbol` entity type joins the registry.
- `symbol` field populated with code-aware tokenizer (§5.2).

**Gates.**
- `bbox_search(query="KnowledgeStore")` returns the struct definition chunk
  + call sites (via `code_content` field).
- `bbox_search(query="trait InspectableEntityProvider")` returns the trait
  definition + impl blocks.
- Symbol table built for this repo without panics; ≥80% of `CALLS` edges
  resolve to a real symbol target.

**Follow-ups.** D2 exposes via `bbox_inspect_entity`. Tier B per-language
deferred to Y-* markers.

---

### Phase S4 — EdgeIndex projection over existing stores

**Scope.** In-memory normalized forward+reverse adjacency, rebuilt at daemon
startup from all stores plus authored sidecars. NO new edge production in
this phase — exposing what's already there.

**Realizes.** Design §5.5, §9.1, §9.4 (provenance edges from existing data).

**Components.**
- `src/edge_index.rs` — `Edge` record + forward + reverse maps.
- Startup pass reads:
  - `KnowledgeEntry.supersedes` → `SUPERSEDES` edges
  - `KnowledgeEntry.links` (new field, see M4) → authored knowledge edges
  - Tantivy transcripts → `IN_SESSION` edges
  - Tantivy project_file chunks → `IN_FILE`, `NEXT_CHUNK`/`PREV_CHUNK` edges
  - Tantivy code chunks (S3) → AST edges (`CALLS`, `IMPLEMENTS_TRAIT`, etc.)
  - `threads.rs` → `THREAD_HAS_SESSION` edges
  - `notes.rs` → `TASK_PRODUCED_NOTE`, `NOTE_FROM_SESSION`,
    `NOTE_IN_THREAD` edges
  - Bro task records → `SESSION_USED_BROFILE`, `ARC_USED_BROFILE` edges
  - `~/.local/state/blackbox/edges/<project_id>.jsonl` sidecar (for
    project_file authored edges) → tool-call provenance + auto-edges
- Optional snapshot persistence at `~/.local/state/blackbox/edges/cache/`
  (skip until rebuild cost is measured).

**Gates.**
- Edge counts on this repo's daemon match expected magnitudes.
- Forward and reverse lookups O(1) after startup pass.
- Daemon startup time after EdgeIndex rebuild < 5s on a typical corpus.

**Follow-ups.** D2 (`bbox_inspect_entity`) consumes EdgeIndex. P1 adds
tool-call provenance edges into the sidecar.

---

### Phase K1 — Knowledge entry indexing

**Scope.** Index `KnowledgeEntry` titles + bodies into tantivy under
`doc_type=knowledge`. Enqueue for embedding via the `knowledge` route.
Without this phase, `bbox_hybrid_search` silently excludes the
highest-value entity type (codex #2).

**Realizes.** Design §6.1 (knowledge entity searchability), §10.2 (type
multipliers reach knowledge), §5.4 (knowledge bucket route).

**Components.**
- Extend reindex thread: scan `~/.claude-shared/blackbox-knowledge.json` on
  startup + on every knowledge mutation hook (`bbox_learn`,
  `bbox_remember`, `bbox_decide`, `bbox_forget`).
- Emit one tantivy doc per entry with `doc_type=knowledge`, `entity_id`,
  `content` (title + body concatenated), `chunk_hash` (sha256 of body).
- Wire knowledge mutation hooks to enqueue embedding via the `knowledge`
  route (queue is no-op until E2 lands).

**Gates.**
- `bbox_search(query="<some knowledge entry phrase>")` returns the entry.
- After E2-E3 land, `bbox_hybrid_search` returns knowledge entries with
  type multiplier applied.
- `bbox_forget` removes entry from tantivy + tombstones embedding.

**Follow-ups.** D1 reads from this index for the `knowledge` provider.

---

### Phase G1 — Git ingestion (commit entity + edges + commit message indexing)

**Scope.** For each registered project under git, ingest commit history
into the corpus. Commits become `commit:<repo_id>:<sha>` entities; commit
messages indexed in tantivy under `chunk_kind=git_message`; commit edges
land in EdgeIndex.

**Realizes.** Design §6.1 (commit entity type), §6.2 (commit refs), §9.6
(git edges), §15.4 (git as part of the graph).

**Components.**
- `src/git/mod.rs` — `gix` (or shell-out) wrapper for `git log`,
  `git blame`, `git notes`.
- Bootstrap arc gains `IngestGitHistory` hook node: walk commit history
  (default: last 1000 commits or all-time, configurable per project),
  emit commit entities + `COMMIT_PARENT`, `COMMIT_BY_AUTHOR`,
  `COMMIT_TOUCHED_FILE` edges.
- Index commit messages into tantivy under `chunk_kind=git_message`,
  `chunk_hash=sha[:12]`.
- Enqueue commit messages for embedding via `git_message` route.
- Subsequent reindex cycles incrementally pick up new commits since the
  last seen HEAD.

**Gates.**
- This repo's commit history present as `commit:*` entities;
  `bbox_search(query="<a recent commit message phrase>")` finds it.
- `bbox_inspect_entity(commit:<repo>:<sha>)` returns message + parents +
  author + files-touched.
- Re-running bootstrap is incremental (existing commits not re-emitted).

**Follow-ups.** P1 emits anchored tool-call edges that `bbox_blame` (P2)
matches against `commit_sha_at_edit`. G2 adds git-notes serialization.

---

## Search surface

### Phase D1 — InspectableEntityProvider trait + per-type adapters

**Scope.** Define the trait + implement adapters for all 10 entity types
plus 2 virtual. No public MCP tool yet; just the providers.

**Realizes.** Design §6.4.

**Components.**
- `src/providers/mod.rs` — trait + registry.
- `src/providers/{knowledge,project_file,transcript,session,thread,note,symbol,brofile,whiteboard,commit}.rs` —
  one per entity type.
- `src/providers/virtual_{task,bash_call}.rs` — virtual providers; resolve
  ref to backing entity and synthesize view.
- Each provider implements: `entity_type`, `owns_ref`, `handles_virtual`,
  `get_entity`, `schema`, `forward_edges`, `expected_edge_families`,
  `recommended_next_hops`, `compact_label`.
- `compact_label` per type: knowledge→title, project_file→first heading or
  first 80 chars, transcript→`role` + first 80 chars, commit→`<sha[:7]> <subject_line>`,
  etc.

**Gates.**
- Round-trip: `EntityRef::parse(s).render() == s` for refs from each provider.
- `compact_label` returns ≤80 chars for every entity type sampled.
- Provider trait method dispatch via `EntityRef::entity_type()` correct,
  including virtual dispatch.

**Follow-ups.** D2 consumes providers via the public facade.

---

### Phase D2 — bbox_inspect_entity + bbox_describe_schema

**Scope.** Two tools. `bbox_inspect_entity` parses entity-ref, dispatches
to provider, renders uniform response (properties + filtered edges +
recommended_next_hops + edge_family_coverage). `bbox_describe_schema`
catalogs entity types AND lists edge type vocabulary with traversal tips
(folds in the daystrom spike's `list_edge_types`).

**Realizes.** Design §4.1 (two tools), §4.4 (error semantics), §6.4.

**Components.**
- `src/mcp_tools/inspect.rs` — facade.
- `src/mcp_tools/describe_schema.rs` — catalogs from registry; dual response
  sections (vertex types + edge families).
- Tool descriptions ported from daystrom AgenticTools with bbox vocabulary
  substitution. Verbatim where possible.
- Response formatting: spike's text-first style with structured fields.
- §4.4 error shapes implemented.

**Gates.**
- Inspecting a `knowledge` entry returns properties + supersedes chain +
  recommended_next_hops including `KNOWLEDGE_FROM_SESSION`.
- Inspecting a `project_file` code chunk returns symbol info + CALLS/CALLED_BY
  + IN_FILE parent.
- `describe_schema` lists 10 entity types + 2 virtual with current
  population counts; lists all edge families with traversal tips.
- §4.4 error shapes verifiable: bad ref → `error.bad_input`; missing →
  `error.not_found` with `similar_refs` populated.

**Follow-ups.** D3 adds path traversal.

---

### Phase D3 — bbox_find_paths + bbox_bundle_evidence + path cache

**Scope.** Direction-preserving BFS. Per-MCP-session monotonic path IDs.
Non-consuming `bbox_bundle_evidence` reads. Path cache as session-scoped
daemon state with `process` mode opt-in via config.

**Realizes.** Design §4.1 (two tools), §5.7.

**Components.**
- `src/mcp_tools/find_paths.rs` — BFS with `max_depth`, `edge_types`
  filter, direction preservation, compact labels inline.
- `src/path_cache.rs` — per-MCP-session LRU bounded ~100, evicts oldest 30.
  `[paths] cache_scope` config supports `"session"` (default) and
  `"process"`.
- `src/mcp_tools/bundle_evidence.rs` — packages
  `(question, entity_refs, path_ids)` into evidence artifact with
  intra-bundle edges + validated path summaries. Handles stale path IDs
  per §4.4 (degraded response).
- Tool descriptions ported.

**Gates.**
- `bbox_find_paths(from=knowledge:abc, edge_types="SUPERSEDES")` returns
  the supersession chain.
- `bbox_find_paths(from=project_file:src/main.rs:chunk-X, edge_types="CALLS")`
  works.
- `bbox_bundle_evidence` round-trips path IDs within the same session.
- Cache LRU eviction confirmed under stress test.
- Stale path IDs reported in `degraded.stale_path_ids`, not as a hard
  failure.

**Follow-ups.** Combined with D1-D2, agentic surface is consumable
end-to-end (sans hybrid).

---

## Embeddings

### Phase E1 — Embedding provider trait + Voyage + Ollama clients + per-bucket routing

**Scope.** Async trait + two implementations + per-bucket route config.
No queue yet; no storage yet; just the provider abstraction + routing.

**Realizes.** Design §5.4.

**Components.**
- `src/embed/mod.rs` — `EmbeddingProvider` trait + `Bucket` enum +
  `Route` resolver.
- `src/embed/voyage.rs` — HTTP via reqwest; `VOYAGE_API_KEY` env;
  `voyage-code-3` default; batched input.
- `src/embed/ollama.rs` — HTTP local; `nomic-embed-text` default.
- Config loader at `~/.config/blackbox/embed.toml` (§5.4): providers,
  routes per bucket, per-project overrides.
- Voyage key seeding: ship with the donor key from
  `daystrom-mk2/deploy/.env` (`DAYSTROM_VOYAGE_API_KEY`) as a fallback;
  user can replace.
- Dim mismatch detection per route.

**Gates.**
- Voyage client returns a vector of correct dim for a known input.
- Ollama client returns a vector of correct dim against a local Ollama.
- Route resolver: `route_for(Bucket::Knowledge)` returns the configured
  provider for the active project_id (with per-project override
  precedence).
- Dim-mismatch refuses to mix; manual `bbox_reembed --route=X` rebuilds.

**Follow-ups.** E2 adds the queue.

---

### Phase E2 — Embedding queue + bbox_embed_status + per-route degradation

**Scope.** Per-route `VecDeque<EmbedRequest>`, debounced batch processing,
content-hash skip, provider fallback per route, rate limiting.
`bbox_embed_status` MCP tool reports per-route status.

**Realizes.** Design §5.4 (queue), §5.8 (vector_status response shape).

**Components.**
- `src/embed/queue.rs` — task per route + queue + debounce.
- Content-hash skip: `chunk_hash` field consulted before enqueueing.
- Rate limiter per provider (config).
- Provider unavailable → that route's queue backs up, never propagate
  to caller.
- `bbox_embed_status` MCP tool: per-route `available`, `indexed_count`,
  `queue_depth`, `last_error` (sanitized).

**Gates.**
- Bootstrapping this repo doesn't block on embedding.
- Re-bootstrap re-uses existing embeddings (content-hash dedup confirmed).
- Voyage outage on the `code` route: that route backs up; BM25 still
  serves; `bbox_embed_status` reports `voyage.available=false`. Other
  routes (e.g. `git_message` if user routed that to Ollama) keep working.
- The bootstrap-arc `EnqueueEmbed` node graduates from no-op to live.

**Follow-ups.** E3 wires queue output to vector storage.

---

### Phase E3 — Vector store: WAL + slab + rebuildable HNSW (multi-partition)

**Scope.** Append-only `records.wal` as canonical store. `slab.bin` /
`ids.bin` / `graph.bin` as rebuildable derived state. SIMD cosine via
`wide::f32x8`. HNSW per erlang-test parameters. **Multi-partition**: one
slab+graph per `(provider, dims)` tuple to support cross-route routing.

**Realizes.** Design §5.3.

**Components.**
- `src/vectors/mod.rs` — public API: `upsert`, `delete`, `search`, `rebuild`.
- `src/vectors/wal.rs` — append-only log; fsync on debounce-window close.
  Records carry `route` field naming partition.
- `src/vectors/slab.rs` — contiguous f32 vectors per partition;
  ordinal-indexed.
- `src/vectors/hnsw.rs` — port from `erlang-test/.../hnsw.rs`. Adapt to
  in-process Rust (no Rustler NIF wrapping). One HNSW per partition.
- `src/vectors/distance.rs` — port from `erlang-test/.../distance.rs`.
  SIMD cosine, dot product helpers.
- Add `wide` dep to Cargo.toml.
- Startup integrity check: validate slab/ids/graph against WAL watermark
  per partition; rebuild from WAL on mismatch.
- Single-writer tokio task per partition.

**Gates.**
- Cold start: WAL → slab+graph rebuild correct on this repo's vector
  count after E2 drains.
- Crash test: kill daemon mid-write; restart rebuilds without data loss.
- Cosine distance via SIMD matches scalar reference within 1e-6.
- HNSW search returns top-k matches comparable to a brute-force baseline
  on a 10k-vector test set per partition.
- Mixing two providers across two routes (e.g. voyage code, ollama
  transcripts) produces two healthy partitions.

**Follow-ups.** H1 wires vector search into hybrid + seed-finder with
multi-partition fusion.

---

### Phase F2b — Eval suite gates resolved

**Scope.** Now that K1 + S2/S3 + G1 + E3 have landed, every query in the
F2a suite has a resolvable expected entity ref. Materialize the refs and
flip the gate from "manifests parse" to "every expected ref resolves to
a known entity."

**Realizes.** Design §16.1 fully.

**Components.**
- For each F2a query manifest, resolve target locators to canonical
  EntityRef strings.
- Verify each resolves via the entity-ref parser + provider lookup.

**Gates.**
- Every query's expected entity ref(s) resolve to a known entity in the
  current daemon's corpus.
- Manual run (LLM-by-hand) of the suite yields a baseline pass rate
  (any number; verifies the suite is runnable).

**Follow-ups.** H3 wraps the suite in the harness.

---

## Hybrid search

### Phase H1 — RRF fusion + bbox_hybrid_search (multi-partition)

**Scope.** RRF formula with canonical entity_id keys. Type-aware
multiplier. Temporal decay for knowledge. Multi-partition vector fusion
(per §10.1). `bbox_hybrid_search` MCP tool.

**Realizes.** Design §10.1, §10.2, §10.3, §4.1 (`bbox_hybrid_search`).

**Components.**
- `src/search/rrf.rs` — fusion formula; HashMap key is `entity_id`;
  per-partition vector ranks fused with BM25 rank list.
- `src/search/rerank.rs` — type-aware multiplier table; temporal decay
  for knowledge entries.
- `src/mcp_tools/hybrid_search.rs` — MCP wrapper; emits per-route
  `vector_status` inline (§5.8).
- Tool description ported.

**Gates.**
- RRF fuses tantivy + per-partition vector ranks correctly on a fixture.
- `entity_id` keys collapse same-entity hits from BM25 and vector to one row.
- Type multiplier verified on a hand-curated query that returns mixed types.
- `vector_status` field surfaces per-route queue depth and availability.
- Cross-route query (e.g. "design doc + code, where docs route to Voyage
  and code routes to Ollama") returns merged results without dim confusion.

**Follow-ups.** H2 layers the seed-finder.

---

### Phase H2 — bbox_discover_seed_entities with notable_edges

**Scope.** Wraps `bbox_hybrid_search` + adds `notable_edges` previews on
top results to cue the agentic loop.

**Realizes.** Design §4.1 (`bbox_discover_seed_entities`).

**Components.**
- `src/mcp_tools/discover_seed.rs` — calls hybrid_search, enriches top-K
  results with one cheap edge preview per result via EdgeIndex.
- Tool description: "search results are seeds, not proof."

**Gates.**
- Top-3 results carry a `notable_edges` preview (≤2 edges each).
- Description text observable in MCP `list_tools`.

**Follow-ups.** Combined with D1-D3, agentic surface is consumable
end-to-end.

---

### Phase H3 — Eval harness shell script + nightly cron arc

**Scope.** Shell-script harness (adapted from
`daystrom-mk2/spikes/run-agentic-eval.sh`) iterates the F2 query suite,
dispatches a stock LLM with bbox tools attached against blackboxd-dev.
Three-strategy comparison (search-only / static hybrid / agentic).
Nightly cron arc invokes the harness.

**Realizes.** Design §16.4, §12.7.

**Components.**
- `eval/run-agentic-eval.sh` — adapted from donor; dispatches per-query
  via stock LLM (Claude Code CLI, Codex CLI, etc.).
- Per-query JSON verdict + scoreboard format (matches donor harness).
- Strategy implementations: search-only, static-hybrid, agentic.
- `examples/agentic-corpus/workflows/nightly-eval-arc.json` —
  cron-triggered.
- `examples/agentic-corpus/packets/eval/drift-policy.json` — `stable |
  drift_minor | drift_major`.
- `bbox_thread(thread-cba8bfa1)` cross-references the `foreach` engine
  follow-up.

**Gates.**
- Suite runs end-to-end against blackboxd-dev.
- Pass rates per strategy reportable.
- Drift detection fires on a synthetic regression.

**Follow-ups.** Tighten gates per design §16.3 once baseline established.

---

## Provenance

### Phase P1 — Tool-call provenance with anchored edges

**Scope.** Extend `src/parser.rs` to recognize Edit/Write/Read/Bash tool
calls; emit anchored edges (§14.2) into the EdgeIndex sidecar at index
time. Anchors carry `file_path`, `byte_range`, `content_hash_at_edit`,
`commit_sha_at_edit`. `bash_call:<session>:<turn>` virtual entity
addressable.

**Realizes.** Design §9.5, §14.2.

**Components.**
- Parser registry: per-provider tool-name → semantic mapping (Claude Code's
  `Edit`, Codex's analogous tools, etc.).
- Anchor construction: at parse time, capture file_path + byte_range from
  tool args + content_hash from current file state + commit_sha from
  `git rev-parse HEAD` if working tree clean.
- Edge emission with `provenance=derived, confidence=exact` for explicit
  file args; `provenance=derived, confidence=heuristic` for bash side
  effects.
- Sidecar: append edges to
  `~/.local/state/blackbox/edges/<project_id>.jsonl`; loaded by EdgeIndex
  startup.
- Reverse projections: `EDITED_BY_SESSION`, `EDITED_BY_BROFILE`,
  `EDITED_IN_ARC`, `EDITED_BY_TRIGGER` (chained through provenance edges
  from S4).

**Gates.**
- Walking back from any chunk in this repo's `src/` returns at least one
  `EDITED_BY_SESSION` edge if any agent has edited it.
- `bash_call:<session>:<turn>` resolves and inspects.
- Anchor survives: editing a file post-anchor doesn't invalidate the edge;
  resolution at query time correctly maps `byte_range` through git blame
  (P2 below).

**Follow-ups.** P2 adds `bbox_blame` which uses anchors.

---

### Phase P2 — bbox_blame derived tool

**Scope.** Convenience MCP tool that walks `EDITED_BY_*` edges + git blame
from a `(file, line)` to produce conversational lineage. Composes
provenance edges with git history (commit_sha_at_edit anchor matching).

**Realizes.** Design §14.3.

**Components.**
- `src/mcp_tools/blame.rs` — input: `(file, line)` or `entity_ref`.
- Walk:
  1. `git blame` on live file at HEAD to find introducing commit.
  2. Match commit_sha against tool-call anchors.
  3. On match, walk session → brofile → arc → trigger.
  4. Include immediately-prior `READ_FILE` calls from same session as
     "informed by reading X, Y, Z".
- Renders chronological chain with brofile + arc + trigger + prior reads.

**Gates.**
- `bbox_blame(file=src/main.rs, line=42)` on this repo returns a non-empty
  chain when the line was edited by an agent.
- Output matches a manually-traced lineage on a known recent edit.
- Gracefully degrades to `git blame` author info only when no anchor
  matches.

**Follow-ups.** G2 makes the chain portable across machines via git notes.

---

### Phase G2 — Git notes serialization for cross-machine provenance

**Scope.** Persist provenance to `refs/notes/bbox/provenance` (and
`refs/notes/bbox/knowledge` for knowledge writes). Bootstrap arc reads
notes on encounter and replays into EdgeIndex + knowledge store.

**Realizes.** Design §15.1, §15.2, §15.3.

**Components.**
- `src/git/notes.rs` — read/write `refs/notes/bbox/*` via `gix` or
  shell-out.
- Per-commit note JSON body (§15.1): `produced_by` (session/brofile/arc/
  trigger), `tool_calls` list, `knowledge_writes` list.
- Hook: on each commit observed by bbox (post-commit hook OR cron-driven
  catch-up), if commit was produced through a tracked arc/session, write
  the note.
- Bootstrap arc: detect `refs/notes/bbox/*` on registered project; if
  present, fetch + replay into EdgeIndex.
- Append-merge driver for `refs/notes/bbox/*` (configurable
  `notes_namespace` per §20.7).

**Gates.**
- Edit a file in this repo through a tracked session, commit, push the
  notes; clone the repo on another machine, run bbox_project_register;
  EdgeIndex picks up the provenance from notes.
- Manual edits to a note's JSON survive append-merge against a divergent
  branch's notes.

**Follow-ups.** Documentation of the notes namespace and merge strategy
for users.

---

## Producer machinery

(Bootstrap arc M1 already landed alongside S2 per the codex-revised
sequencing. Arcs below are additive.)

### Phase M2 — Compaction arc

**Scope.** Cron-driven workflow per partition. Reads per-route
vector_status, computes deleted-ordinal ratio, decides
compact/notify/skip via packet, executes rebuild on `compact`.

**Realizes.** Design §12.5.

**Components.**
- `examples/agentic-corpus/workflows/embed-compaction-arc.json`.
- `examples/agentic-corpus/packets/embed/compaction-policy.json` — lattice
  `compact | notify | skip`.
- Hook ops: `read_vector_status`, `quiesce_search`, `rebuild_hnsw` (calls
  into E3 rebuild per partition), `swap_atomic`.
- Cron-inlet schedule: nightly default.

**Gates.**
- Synthetic test per partition: mark 40% of vector ordinals deleted,
  run arc, verify compaction.
- `notify` verdict surfaces via `bbox_inbox`.

**Follow-ups.** H3's nightly eval arc shares the cron-inlet machinery.

---

### Phase M3 — Auto-digest arc + classifier-bro brofiles + per-bucket policy

**Scope.** Workflow triggered by `task-completed` signal. Classifier bro
proposes knowledge entries. Quality gate packet decides `auto_apply` /
`hold_for_review` / `reject`. Operator-audited gate; rendered entries
always hold; daily cap configurable (high default per A6).

**Realizes.** Design §12.2, §13.

**Components.**
- `examples/agentic-corpus/workflows/auto-digest-arc.json`.
- `examples/agentic-corpus/packets/auto-digest/entry-quality.json` —
  `auto_apply | hold_for_review | reject`. Constraints: rendered entries →
  always hold; missing provenance → reject; daily cap via counter
  predicate (default 50, configurable).
- `examples/agentic-corpus/packets/bro-trust/per-brofile.json` — composed
  by entry-quality gate via `Apply{packet=bro-trust, expect=trusted}`.
- Brofile: `digest-extractor` (Sonnet 4.6 with extraction-focused lens).
- Hook ops: `read_session`, `parse_json`, `validate_schema`,
  `apply_entry` / `surface_to_inbox` / `log_reject`.
- Routing packet: `task-completed` signal → `start_arc auto-digest-arc`
  for task_kinds where digest applies.

**Gates.**
- Audit cycle: 20 (proposal, expected_verdict) curated examples; gate
  packet hits ≥18/20 fidelity before going live.
- `auto_apply` count per day measurable; cap enforced.
- Bad brofile flagged via `bro-trust` packet → entries from that brofile
  forced to `hold_for_review` regardless of content.

**Follow-ups.** M4 (contradiction review) handles entries that surface
contradictions.

---

### Phase M4 — Contradiction-review arc + KnowledgeEntry.links field + whiteboard specialists

**Scope.** Tier-0 cosine > 0.85 detection during embed write opens a
whiteboard. Three specialist brofiles + operator slot. Facilitator emits
structured verdict; gate packet routes link writes onto
`KnowledgeEntry.links`.

**Realizes.** Design §12.3, §5.5 (KnowledgeEntry.links field).

**Components.**
- Extend `KnowledgeEntry` schema with `links: Vec<KnowledgeEdge>` field
  (additive, default empty).
- `examples/agentic-corpus/workflows/contradiction-review-arc.json`.
- `examples/agentic-corpus/packets/contradiction/review-synthesis.json` —
  `contradicts | supersedes | tension_with | related | no_conflict | hold`.
- Three brofiles:
  - `contradiction-provenance` (Sonnet 4.6 + provenance lens).
  - `contradiction-lifecycle` (Sonnet 4.6 + lifecycle lens).
  - `contradiction-coherence` (Sonnet 4.6 + coherence lens).
- Trigger: synchronous embed-write detects cosine > 0.85 vs another entry
  not in supersession chain → `contradiction-detected` signal → routing
  packet → start_arc. **If arc not installed: signal goes to bbox_inbox
  via fallback `bbox_note(kind=surprise)`.**
- Hook ops: edge writers per verdict — `append_knowledge_link(target,
  kind, note, source_arc)`.

**Gates.**
- Synthetic contradiction (two entries with cosine > 0.9, opposing claims)
  → arc opens board → specialists post → operator votes → verdict applied
  to `KnowledgeEntry.links`.
- Operator joining the board as `agent_name=operator` works.
- Without the arc installed, tier-0 detection still surfaces via
  `bbox_inbox` (fallback path).

**Follow-ups.** M5 (auto-edge) handles non-contradiction semantic edges.

---

### Phase M5 — Auto-edge-extraction arc — DESCRIBES + REFERENCES

**Scope.** Ensemble of classifier bros vote on candidate semantic edges.
Aggregation packet decides write/hold/reject. v1 scope: `DESCRIBES`
(design-doc section ↔ code symbol) and `REFERENCES` (knowledge entry ↔
code/file). Structural code edges (CALLS, IMPLEMENTS_TRAIT) remain
deterministic AST extraction (S3); this arc is for edges requiring
judgment.

**Realizes.** Design §12.4.

**Components.**
- `examples/agentic-corpus/workflows/auto-edge-arc.json`.
- `examples/agentic-corpus/packets/auto-edge/vote-aggregate.json` —
  `write_edge | hold_for_review | reject`. Aggregation rules: ≥2 of 3
  voters agree → write; 1 voter → hold; 0 → reject.
- Brofiles per edge kind:
  - For `DESCRIBES`: `describe-prose-signal`, `describe-symbol-fit`,
    `describe-narrative-cohesion`.
  - For `REFERENCES`: `reference-citation-precision`,
    `reference-target-existence`, `reference-context-fit`.
- Trigger: scheduled scan (cron-inlet) over candidate pairs (markdown
  doc_section chunks ↔ code symbols in same project_id).
- Hook ops: edge writers append to `KnowledgeEntry.links` (for
  REFERENCES) or sidecar `<project_id>.jsonl` (for DESCRIBES, where
  source is project_file_chunk).

**Gates.**
- Audit cycle: 15 (candidate, expected_verdict) curated examples per edge
  kind; aggregation packet hits ≥12/15 fidelity per kind before going live.
- Synthetic candidate (a known doc-section ↔ symbol pair) produces
  `write_edge` and the edge appears in EdgeIndex.

**Follow-ups.** Additional semantic edge kinds opt in via new arcs +
brofiles + packets following the same pattern.

---

## Multimodal expansion `[markers, prioritized]`

The chunker registry from S2/S3 makes per-format chunkers additive. Each
phase below adds one format; none block the others. Ordered by user
priority (PDF first; HTML / AV / IMG late).

### Phase X-PDF — PDF chunker `[marker]`

**Scope.** `pdf-extract` for text PDFs; `tesseract` shell-out for scans.
Emits `pdf_page`, `pdf_figure`, `pdf_table` chunks. Edges: `ON_PAGE`,
`FIGURE_OF`, `TABLE_OF`, `CITATION_TO`.

### Phase X-IPYNB — Jupyter notebook chunker `[marker]`

**Scope.** Cell-level chunks with cell index + outputs. Edges:
`NEXT_CELL`, `OUTPUT_OF`, `IMPORTS_FROM_CELL`.

### Phase X-XLSX — Spreadsheet chunker `[marker]`

**Scope.** `calamine` crate. Sheet-level + cell-range chunks. Edges:
`IN_SHEET`, `COMPUTED_FROM` (formula deps), `CELL_REFERENCES`.

### Phase X-DOCX-PPTX — Office documents chunker `[marker]`

**Scope.** `docx-rs` / `pptx` parser. Edges: `IN_SECTION`, `ON_SLIDE`,
`IN_DECK`.

### Phase X-HTML — HTML / web archive chunker `[marker]`

**Scope.** `scraper` crate. Edges: `LINKS_TO_URL`, `EMBEDS_FRAME`.

### Phase X-AV — Audio/video transcript chunker `[marker]`

**Scope.** Time-segmented chunks from external transcript producers
(whisper). Edges: `AT_TIMESTAMP`, `IN_RECORDING`.

### Phase X-IMG — Standalone image chunker `[marker]`

**Scope.** VLM caption extraction. Requires multimodal embedding model
(open question §20.3). Edges: `DEPICTS`, `CAPTIONED_AS`.

---

## AST depth `[markers]`

Per-language LSP-style integration replacing naive symbol-table CALLS
resolution. Target language set: rust, python, csharp, java, go,
typescript, javascript, c, cpp. Order opportunistic; Rust first as
largest representation in corpus.

### Phase Y-Rust — Tier B Rust via rust-analyzer `[marker]`
### Phase Y-Python — Tier B Python via pyright `[marker]`
### Phase Y-CSharp — Tier B C# via omnisharp `[marker]`
### Phase Y-Java — Tier B Java via jdtls `[marker]`
### Phase Y-Go — Tier B Go via gopls `[marker]`
### Phase Y-TS — Tier B TypeScript / JavaScript via tsserver `[marker]`
### Phase Y-CCpp — Tier B C / C++ via clangd `[marker]`

Mark all listed languages as future targets per A4. Build-prop selects
which compile in.

---

## Hardening

### Phase H5 — Response size caps + observability hardening + security policy

**Scope.** Hard caps on every MCP response. `bbox_inbox` surfaces all
hold queues. Eval-flake handling. Packet audit examples shipped with each
packet. Per-bucket data-export policy (per A1) is config-discoverable and
visible in `bbox_embed_status`.

**Components.**
- Audit per-tool response sizes; trim aggressively in
  `bbox_inspect_entity` and `bbox_find_paths` outputs (existing 80KB
  pattern extended to new tools).
- Extend `bbox_inbox` to surface: hold_for_review knowledge entries, open
  contradiction-review whiteboards, eval drift alerts, failed bro tasks,
  tier-0 contradictions when contradiction-review-arc isn't installed.
- Each shipped packet gets an `audit_examples.json` alongside it; CI runs
  `bbox_audit` on each example set.
- `bbox_embed_status` reports per-route `(provider, model, dim)` so
  operator can audit data-export at a glance.
- Documentation: per-bucket route policy guide; example
  privacy-conscious config (everything to Ollama); security caveats for
  Voyage-routed buckets.

**Gates.**
- No MCP response exceeds 80KB on this repo's corpus during a full eval run.
- `bbox_inbox` surfaces every category mentioned above.
- Packet audits pass in CI.
- `bbox_embed_status` output enumerates every active route with its
  provider.

---

## Phase summary

Foundation: **F1, F2a, F3, F4** (4 phases)
Storage substrate: **S1, S2, S3, S4, K1, G1** (6 phases)
Search surface: **D1, D2, D3** (3 phases)
Embeddings: **E1, E2, E3** (3 phases)
Eval suite gates: **F2b** (1 phase)
Hybrid search: **H1, H2, H3** (3 phases)
Provenance: **P1, P2, G2** (3 phases)
Producer machinery: **M2, M3, M4, M5** (4 phases; M1 absorbed into S2)
Multimodal expansion: **X-PDF, X-IPYNB, X-XLSX, X-DOCX-PPTX, X-HTML, X-AV, X-IMG** (7 markers)
AST depth: **Y-Rust, Y-Python, Y-CSharp, Y-Java, Y-Go, Y-TS, Y-CCpp** (7 markers)
Hardening: **H5** (1 phase)

**Totals:** 38 phases, 14 markers (7 X-* + 7 Y-*).

Markers sit at known dependency positions but await detail fleshing.
Deliberation rounds will pick which to flesh out before implementation;
the rest stay as markers until upstream phases land and the surface area
clarifies.

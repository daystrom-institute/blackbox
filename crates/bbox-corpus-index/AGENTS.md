# bbox-corpus-index — tantivy schema, ingest passes, transcript search

## Schema lifecycle

- Bumping `INDEX_SCHEMA_VERSION` drops the ENTIRE transcript index at the
  next daemon open; the reindex IS the backfill. This is the deliberate
  migration pattern (base_project_id shipped this way — 18.6k files / 1.26M
  docs re-stamped in one pass). Budget minutes of rebuild and a window of
  empty search after deploy; never attempt dual-reading old docs instead.

## base_project_id stamping (gap-72fd5932)

- Every transcript/tool_call doc carries the resolved base project id
  alongside the literal session cwd in `project`. Resolution happens once
  per SESSION FILE, memoized per distinct cwd on `ToolEdgeContext` — git
  probes stay bounded by checkout count, not session count. Do not move
  resolution per-doc, and do not move it to query time: per-candidate git
  probes at query time is precisely what the gap rejected.
- The project filter is OR(legacy substring lane over `project`, exact term
  on `base_project_id`). **Never drop the substring lane** — unregistered
  projects and ad hoc path filters have nothing else.
- Selector resolution does NOT live in this crate: callers hand search,
  cite, and sessions_list a `ProjectFilterInput { project_id, literal }`
  resolved at the daemon tool boundary, and the id lane fires only when
  the caller resolved one. Never read project records off disk here to
  interpret a filter: the dependency direction forbids reaching the
  resolver engine, which is why resolution moved up.
- `bbox_sessions_list` is METADATA-backed, not doc-backed: it matches
  candidate session cwds against the stamped documents at query time. If
  session metadata ever gets stamped at write time, that lane can go.

## Filters generally

- A registered selector that silently falls through to the deterministic
  path-hash id derives a FOREIGN id and returns empty results with no error
  — the worktree case made history lanes invisible for weeks. When touching
  filter resolution, keep "registry first, hash fallback last" and test the
  out-of-tree worktree path explicitly.

## Pinned provenance target resolution

- Authenticated provenance import resolves legacy V1 path/range targets only
  against the journal-pinned collected selector and a pinned Tantivy searcher.
  The resolver is exact on project id, selector, and repository-relative path;
  it never consults the live checkout or silently crosses into another active
  code generation while an import is being prepared.
- V2 target membership accepts either an entity in that exact active selector
  or an exact historical target still backed by a matching observed
  `READ_FILE`/`EDITED_FILE` edge in the pinned edge view. The historical arm is
  required across collected-snapshot and ProjectFileV2 migrations; imported
  provenance edges cannot authorize it recursively.

## Graph vertex documents (M9, design/connectors/unified-retrieval.md)

- Every graph vertex document, published AND provisional, carries a stamped
  `project_id` field, and the project filter for graph vertices resolves on
  that field. Never resolve a provisional vertex's project by parsing its
  ref (it carries `scope_hash` + `checkout_id`, no project id) and never by
  a view-catalog lookup inside the query filter: that is a per-hit cost and
  the lock-ordering hazard `src/project_graph_read.rs` documents. Same shape
  as `knowledge_scope_hash` / `knowledge_checkout_id`. M9a lays the field
  down before anything filters on it so M9c is a filter change, not a
  schema bump (operator ruling 2026-08-16, Q6).
- Graph vertices ride the existing BM25 and vector lanes; they add no RRF
  list. A dedicated graph lane is a metrics sweep, not a feature.
- The graph identity fields (`graph_vertex_type`, `graph_id`, `graph_source`,
  `graph_source_connector`, `graph_generation`) must keep flowing through
  `properties_from_doc`: that projection is the only surface inspect and
  hybrid search read, and a stored-but-unprojected field is invisible on
  every response.
- The word-lane graph authority filter is composed into the BM25
  `BooleanQuery` BEFORE `TopDocs`, never post-fusion. A post-filter turns an
  authorization boundary into a silent relevance perturbation: unreadable
  documents would consume rank positions and shift the RRF scores of the
  survivors.
- Tantivy footgun hit while building the project-scope clause: a
  `BooleanQuery` whose clauses are all `MustNot` matches NOTHING (Lucene
  semantics, not "everything except"). The non-graph arm of an OR needs an
  explicit `AllQuery` conjoined with the `MustNot`.
- Graph documents never join the file dedup/aggregation lanes: a graph vertex
  has no file, and a dedup key that fell back to `entity_id` would make every
  vertex its own singleton file group. `file_dedup_key` returning `None` for
  graph refs is the invariant, not an accident of prefix matching.
- The vector lane's graph authority is the per-hit mirror of the word lane's
  query clause (`retain_authorized_graph_vectors` in bbox-mcp-tools), fed by
  `GraphWordPolicySnapshot.embed_lanes`: the accepted generation of every
  lane that embeds, pinned as an `Arc` before the search starts. The
  re-check is "is this vertex still embed-eligible on the pinned
  generation" via `bbox_project_graph::vertex_embed_text`, which is also the
  enqueue-time and backfill-time projection; one function, three consumers,
  so they cannot disagree. The embed projection is NOT stored in the word
  index (documents carry label + `index: text` values only): never try to
  rebuild graph vectors from a tantivy scan.

## Transcript read authority

- Native context/messages/session/topics reads use exact stored transcript
  locators or session ids. A locator may resemble a host path, but read APIs
  never open it or reconstruct source files from it. Source discovery and
  ingestion belong to adapters with explicit roots or enrolled transport.
- Native message bodies are retained index projections and may already have
  parser truncation. Response-preview truncation is a separate fact; neither
  a successful lookup nor the last indexed timestamp establishes complete or
  current source ingestion. Keep this limitation visible in read responses.
- Slack context/messages reads retain their scoped landing-store authority.
  Do not send Slack locators through native index fallback when that authority
  refuses them.
- Native message pagination is ordered by locator and source byte offset.
  A byte-limited page advances by returned rows, including from_end pages;
  never advance by the requested count when fewer rows fit.

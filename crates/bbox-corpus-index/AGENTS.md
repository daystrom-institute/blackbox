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

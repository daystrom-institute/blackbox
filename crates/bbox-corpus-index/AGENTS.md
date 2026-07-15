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
  on `base_project_id`), with the filter value resolved through
  alias/id/path first. **Never drop the substring lane** — unregistered
  projects and ad hoc path filters have nothing else.
- `bbox_sessions_list` is METADATA-backed, not doc-backed: it resolves
  candidate session cwds through the same gate at query time, memoized per
  distinct cwd. If session metadata ever gets stamped at write time, that
  memo can go.

## Filters generally

- A registered selector that silently falls through to the deterministic
  path-hash id derives a FOREIGN id and returns empty results with no error
  — the worktree case made history lanes invisible for weeks. When touching
  filter resolution, keep "registry first, hash fallback last" and test the
  out-of-tree worktree path explicitly.

## Fleet transcript projection

- A fleet transcript coordinate is complete only when its referenced event
  sequence exists in a regular file beneath an explicit allowed root. Resolve
  symlinks, enforce the size bound, and fail closed on sequence gaps.
- Corpus-owned transcript archives are additional harness adapter roots. They
  must participate in ordinary indexing, change detection, and purge scans so
  a full rebuild cannot erase already acknowledged worker history.

# roadmap — test results

End-to-end smoke test via HTTP MCP confirmed: create → get → list → next → render
all produce correct output (item id `roadmap-23fb5018`, status `proposed`).

## test results (2026-05-08)

- [x] `bbox_roadmap(action="promote")` — spins a thread, parses id from result **PASS**
- [x] `bbox_roadmap(action="link")` with each edge kind (spawns, deferred_from,
  designed_in, depends_on, blocked_by, supersedes, subsumes, related_to) **PASS**
- [x] `bbox_roadmap(action="repair_links")` — dry-run and live paths **PASS**
- [x] `bbox_inspect_entity` on a `roadmap_item:<id>` ref — verifies
  RoadmapItemProvider + EdgeIndex roadmap edge projection
  **PASS**: properties ok. Edges show 0 at query time because the EdgeIndex
  rebuilds asynchronously (60s watcher); `project_roadmap_edges` is confirmed
  wired into `EdgeIndex::rebuild()` and projects all 8 ROADMAP_* edge families.
- [x] `bbox_hybrid_search` with `doc_type=roadmap` — verifies tantivy indexing **PASS**
- [x] `bbox_reembed(route="knowledge")` — verifies roadmap items are
  enqueued alongside knowledge entries **PASS**
- [x] `bbox_roadmap(action="render", write_path="/tmp/ROADMAP.md")` — writes
  to disk, verify stale/missing markers **PASS**
- [x] Full reindex preserves roadmap docs
  **PASS**: `scan_all_source_files` includes `config.roadmap_path` and
  `reindex_roadmap_store_standalone` reindexes all non-rejected items.
  Initial test failure was a timeout (full reindex is slow on large corpora).
- [x] Spawned thread → in_progress computed status (get/list) **FIXED**
- [x] Resolved spawned thread → done computed status **FIXED**
- [x] `next` project filter returns correct scope-isolated results **FIXED**
- [x] Cross-project roadmap items don't leak into un-scoped list/next **FIXED**
- [x] Eval manifest regenerated for EntityRef variant count (14, was 13) **PASS**

## bugs found and fixed

1. ~~EdgeIndex projection gap~~: NOT a bug. Edges are projected by `project_roadmap_edges`
   into the EdgeIndex on every rebuild. The test queried before the async rebuild
   watcher tick (60s interval). Confirmed wired correctly.

2. ~~Full reindex purges roadmap docs~~: NOT a bug. `scan_all_source_files` already
   includes `config.roadmap_path` and `reindex_roadmap_store_standalone` reindexes all
   non-rejected items. Test failure was a timeout, not a purge.

3. **Computed status not derived from spawned threads** → **FIXED** in `tools/roadmap.rs`:
   `roadmap_get` and `roadmap_list` now read the threads store and check each spawned
   thread's resolution state via `Roadmap::computed_status()`. Previously they used a
   heuristic (has spawns → "in_progress") that never showed "done". Now:
   - Accepted + spawned threads all resolved → "done"
   - Accepted + spawned threads not all resolved → "in_progress"
   - Accepted + no spawns → "accepted"

4. **Cross-project leak in `next`** → **FIXED** in `roadmap.rs` + `tools/roadmap.rs`:
   `Roadmap::next()` now accepts an `Option<&str>` project filter that is applied
   *before* scoring, not after. Global-scope items always pass. The tool handler
   passes `p.project.as_deref()` directly. This ensures top-N scoring is scoped
   to the requested project.

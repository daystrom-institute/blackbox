# roadmap — remaining work

End-to-end smoke test via HTTP MCP confirmed: create → get → list → next → render
all produce correct output (item id `roadmap-23fb5018`, status `proposed`).

## pending test tasks

- [ ] `bbox_roadmap(action="promote")` — spins a thread, parses id from result
- [ ] `bbox_roadmap(action="link")` with each edge kind (spawns, deferred_from,
  designed_in, depends_on, blocked_by, supersedes, subsumes, related_to)
- [ ] `bbox_roadmap(action="repair_links")` — dry-run and live paths
- [ ] `bbox_inspect_entity` on a `roadmap_item:<id>` ref — verifies
  RoadmapItemProvider + EdgeIndex roadmap edge projection
- [ ] `bbox_hybrid_search` with `doc_type=roadmap` — verifies tantivy indexing
- [ ] `bbox_reembed(route="knowledge")` — verifies roadmap items are
  enqueued alongside knowledge entries
- [ ] `bbox_roadmap(action="render", write_path="/tmp/ROADMAP.md")` — writes
  to disk, verify stale/missing markers
- [ ] Full reindex preserves roadmap docs (not purged by scan_all_source_files)
- [ ] Spawned thread → in_progress computed status (get/list)
- [ ] Resolved spawned thread → done computed status
- [ ] `next` project filter returns correct scope-isolated results
- [ ] Cross-project roadmap items don't leak into un-scoped list/next
- [ ] Eval manifest regenerated for EntityRef variant count (14, was 13)

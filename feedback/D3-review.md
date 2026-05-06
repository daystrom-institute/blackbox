# D3 + D1 fixes review

Commits `e52ce37..1f327c8` (4 D1 fixes + D3 find_paths/bundle_evidence/path_cache).

## Issues (fix-forward)

1. **Path cache eviction is FIFO, not LRU.** Design §5.7 specifies
   "bounded LRU ~100 paths, evicts oldest 30 on overflow." `path_cache.rs`
   uses `VecDeque::push_back` + `pop_front` — that's FIFO by insertion.
   With non-consuming reads from `bundle_evidence`, a frequently-accessed
   P1 still gets evicted at insertion overflow because LRU isn't
   tracked. Either:
   - Add an `accessed_at` timestamp per cached path; on `get`, update
     it; on overflow, sort by accessed_at and evict the oldest 30.
   - Switch to `lru` crate (well-known, small dep).
   - Document explicitly that the cache is FIFO, not LRU, and the
     design's LRU constraint was wrong.
   Impact at v1 scale (≤100 paths) is small; FIFO/LRU converge when
   reads happen close to inserts.

2. **`bundle_evidence` fails the WHOLE bundle on the first
   unresolved entity_ref** — returns `error.not_found` and stops.
   Per design §4.4 the more graceful pattern is `status: ok` with
   `degraded.unresolved_entity_refs: [...]`. Today, a bundle of 5
   refs where 1 is missing fails entirely instead of returning the
   4 it COULD resolve. Refactor: collect resolution errors into a
   `degraded` field; only return `error.not_found` if ALL refs fail.

3. **Clippy baseline jumped 98 → 102** during D3 work. Codex says
   "no diagnostics in the new D3 files" — so the new errors are
   elsewhere. Investigate. Likely candidates: D1 fix #1's
   per-provider data-loading code may have triggered some new
   clippy lints; or the F4 fix #4 (lift workflow capability
   validation) bumped a count; or the new `mcp_tools/*` modules
   carry an `#[allow(dead_code)]` in non-D3 code that fires now
   that we use these via D3. Run `cargo clippy --bin blackboxd
   2>&1 | grep -c error:` and bisect against the pre-D3 commit.

## Concerns

4. **Process-wide path cache.** Codex flagged in done note;
   acceptable per the prompt's fallback. The cache internals are
   keyed (per-session structure), so swap-in is non-breaking when
   rmcp exposes session id. Flag for revisit when MCP session-id
   access lands.

5. **`expansions` walks both forward and reverse edges and pushes
   them as `(kind, direction, neighbor)` tuples.** Inside one path,
   you can therefore go OUT then IN (mixed direction). For some
   edge families this is fine (e.g. transcript IN_SESSION → session
   reverse THREAD_HAS_SESSION → thread); for others it's a category
   error. Defer; revisit if path-finding produces nonsense paths in
   practice.

6. **`render_path` calls `render_node` per step** which goes through
   `provider_for(...).compact_label(...)`. After D1 fix #1 lands,
   compact_label has real titles. But D2 fix #2 (compact_label
   consults loaded entity) hasn't landed yet — so render_path uses
   stub-era labels until D2 fix #2 ships in the next leapfrog. Note
   for next cycle: D3's path-rendering quality improves automatically
   when D2 fix #2 lands.

## D1 fix observations

7. **D1 fix #1 (load providers from backing stores)** — major
   architectural fix. `ProviderContext` arg added to trait;
   data-loading pushed INTO each provider. main.rs shrunk by 115
   lines. Each provider now has 14-40 lines of real lookup code.
   This is what D1 should have shipped originally. ✓

8. **D1 fix #2 (drop forward_edges)** — dead trait method
   eliminated. ✓

9. **D1 fix #3 (neighborhood ownership doc)** — trait contract
   documented: caller fills `entity.neighborhood` from EdgeIndex
   before calling `recommended_next_hops`. ✓

10. **D1 fix #4 (rename empty view helper)** — `base_view` →
    `empty_neighborhood_view`. ✓

## Nits

11. **`find_paths` BFS uses `entry.visited.clone()` per
    neighbor expansion** — quadratic memory in the worst case.
    For depth-3 paths with branching factor 5, ~125 visited-sets
    per path. At v1 scale this is fine; flag if depths > 5 ever
    become legal.

12. **`EdgeTypesParam` accepts EITHER `String` (comma-separated)
    OR `Vec<String>`** via `#[serde(untagged)]`. Nice ergonomic;
    callers can pass either shape. Document in tool description.

13. **`render_path` uses Markdown arrows `--KIND-->` and
    `<--KIND--`** matching the daystrom AgenticTools spike. Direction
    preservation is correct — each step renders with the actual
    traversal direction. ✓

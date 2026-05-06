# S4 + S2 fixes review

Commits `acdfa65..a51967e` (4 S2 fixes + S4 EdgeIndex projection).

## Issues (fix-forward)

1. **`event_idx: 0` hardcoded in transcript IN_SESSION edges**
   (`edge_index.rs:240`). The current tantivy schema doesn't store
   `event_idx`, and codex defaulted to 0. So every transcript event on
   the same JSONL line gets the SAME source entity_ref —
   `transcript:<provider>:<session>:<line_offset>:0`. This recreates
   the S1 review's #4 finding (byte_offset uniqueness collision)
   inside the EdgeIndex layer. Either:
   - Add an `event_idx` tantivy field and populate it during transcript
     indexing (would touch F3's schema; bumps `INDEX_SCHEMA_VERSION`)
   - Or skip emitting IN_SESSION edges per-event and only emit
     session→IN_PROJECT or similar coarser-grained edges
   - Or accept the collision for v1 with a code comment
   Pick one and document.

2. **`edge_projection_docs` loads ALL tantivy docs into memory** in one
   `AllQuery + TopDocs::with_limit(num_docs)` call. For this repo (~45
   transcripts, modest size) this is fine — 11k edges in 58ms is great.
   For a real-scale corpus (100k+ docs), this allocates a Vec the size
   of the entire index AND blocks startup. Either:
   - Stream via `searcher.search(&AllQuery, &CountCollector)` first,
     then iterate via paginated `TopDocs::and_offset`
   - Use `IndexReader::searcher().segment_readers()` to walk segments
     directly without materializing scored hits
   The current shape works at small scale; flag for scale-out before
   the corpus crosses ~50k docs.

3. **`HashSet<Edge>` for dedup keyed on the full Edge struct.** With
   11k edges this is fine; with a million it's a lot of cloning during
   `insert` (`if !seen.insert(edge.clone())`). The `Edge` struct
   contains EntityRef-with-Strings, so each insert allocates. For
   future scale, key the HashSet on a hash of the edge instead of the
   edge itself. Defer; flag.

## Concerns

4. **Bro task projection (`SESSION_USED_BROFILE`) skips entries where
   `bro_label.contains("::")` or `session_id == "pending"`.** The
   `::` filter excludes named bros from teams (which use `team::bro`
   notation). The `pending` filter is reasonable. But a TEAM-dispatched
   bro is exactly the case where SESSION_USED_BROFILE is most useful
   for provenance tracking. Either parse the team::bro form and emit
   correctly, or surface in done note that team-dispatched session→
   brofile edges are missing in v1.

5. **`edges_dir_from_bro_store` and `edges_dir_from_projects_path`**
   are two separate path-derivers. They produce the same path
   (`<state>/edges/`) for typical layouts but diverge under override.
   Consolidate into one function `edges_dir(state_dir)` once the
   state-dir invariant is established.

6. **`Edge::metadata` field added** (BTreeMap<String, String>) but
   nothing populates it yet. P1 (tool-call provenance with anchored
   edges) needs metadata fields like `byte_range`, `commit_sha_at_edit`,
   `content_hash_at_edit`. The shape is forward-compatible with that.
   Good; just note that v1 stores empty metadata for everything.

7. **`#[allow(dead_code)]` on the EdgeIndex query methods.** Same
   pattern as F3 fields — they'll be exercised once D2/D3 land. Track
   the suppression in release notes so each consumer phase removes the
   allow when it lights up.

## S2 fix observations

8. **S2 fix #1 (markdown self-loop edges)** — landed cleanly. The
   `derive_edges` loop that pushed self-loops is gone. NEXT_SECTION
   still emitted correctly. The `// TODO(S4+):` comment is in place.

9. **S2 fix #2 (populate config chunk offsets)** — chose to populate
   offsets across all chunkers (config, toml, yaml, text) rather than
   the alternative of dropping byte_start/byte_end. Acceptable choice;
   keeps the `byte_offset` tantivy field meaningful.

10. **S2 fix #3 (centralize git helpers)** — created `src/git.rs`
    module owning all git invocations. `entity_ref.rs` and
    `project_files.rs` both call into it. Unblocks G1 cleanly.

11. **S2 fix #4 (verify JSON chunk dedup)** — added a test that
    confirms `chunk_hash` differs between non-canonical formats of
    the same logical content. Documented behavior: chunkers serialize
    via `serde_json::to_string_pretty` so re-runs on unchanged source
    produce stable hashes IFF the source already matches the canonical
    form. Edge case acceptable; flag.

## Nits

12. **`edge_count()` sums forward map only.** Reverse map should
    have the same total (every edge is in both). Worth a debug_assert
    or just inline the constant lookup.

13. **`exact_edge` is the only Edge constructor used in S4.** No
    heuristic edges yet (those land in P1 / M5). When heuristic
    edges arrive, having a `derived_edge` constructor parallel to
    `exact_edge` will make the call sites symmetric. Not urgent.

14. **`project_sidecar_edges` reads sidecars per project_id directory
    listing, but only handles `*.jsonl` files.** That's fine; the
    convention is one JSONL per project. But the function silently
    skips directories matching the wrong pattern. Add a debug log
    for any non-jsonl files encountered for diagnostic visibility.

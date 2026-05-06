# P2 + H3 fixes review

Commits `6b4a025..9a0764f` (3 H3 fixes + P2 bbox_blame).

## Issues (fix-forward)

1. **`matching_anchors` walks `edge_index.all_edges()` per blame call.**
   For a corpus with 10k+ edges this is O(n) per request. Add an
   index keyed by `commit_sha_at_edit` to EdgeIndex (or a separate
   `commit_anchor_index: HashMap<String, Vec<usize>>` projection
   built at startup) so anchor lookup is O(1).

2. **`prior_read_edges` also walks `edge_index.all_edges()`** to
   find READ_FILE in the same session. Same scaling concern.
   Either:
   - Index READ_FILE edges by `(provider, session_id)` tuple.
   - Walk the session's transcript range via existing tantivy
     filter, then check tool-call status per event.
   Defer; flag.

3. **`prior_reads` is bounded to the 5 most recent before edit but
   doesn't bound by time-since-edit.** A session that read 1000 files
   between turn 1 and the edit at turn 950 surfaces reads from turns
   945-949 — recent but not necessarily related. Add an additional
   bound: only reads within the last N turns (say 20) of the edit.

## Concerns

4. **`resolve_target` requires either entity_ref OR file+line.** The
   error path when both are absent could be more helpful: "Provide
   either `entity_ref` (project_file ref) or `file` + `line`."
   Verify the bad_input message guides the user.

5. **`line_for_byte_offset` reads the file from disk** to map byte
   offset to line number. For large files (>2MB) this is wasteful;
   the chunk's byte_start is already in the entity. If we stored
   line_start alongside, we'd skip the file read entirely. Defer.

6. **bbox_blame doesn't consult `bbox_inspect_entity`'s structured
   response.** Both tools surface entity provenance; they could
   share a common `provenance_for(entity_ref)` helper. Defer the
   refactor; flag.

## H3 fix observations

7. **H3 fix #1 (document nightly arc limits)** — release notes
   updated to explain that nightly-eval-arc records but doesn't
   route through gate. ✓

8. **H3 fix #2 (isolate eval LLM worktree)** — `EVAL_USE_WORKTREE=1`
   default; LLM runs in `/tmp/agentic-eval-worktree-<TIMESTAMP>`.
   Cleanup at end. ✓

9. **H3 fix #3 (synthetic regression target once)** — fixed via
   `EVAL_SUITE_FIRST_ID` env propagation. Inspect once, inject for
   matching ID only. ✓

## Nits

10. **`render_text` builds the chain in markdown.** Sample output
    looks clean. The "informed by prior reads" line includes path
    + turn; consider compact_label for richer identification.

11. **`bbox_blame` MCP tool description** should explicitly note the
    "anchor-matching" vs "git-only fallback" two-mode behavior so
    callers know whether they're getting bbox provenance or just
    git author info.

12. **No unit test for the git-only fallback path.** Add a fixture
    with a commit that has no tool-call anchor (e.g. a non-bbox
    commit) and assert blame returns the git-author-only response.

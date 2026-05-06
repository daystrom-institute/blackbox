# P1 + H2 fixes review

Commits `d0cd720..aeed4a5` (3 H2 fixes + P1 tool-call provenance).

## Issues (fix-forward)

1. **`line_offset_to_turn(line_offset, event_idx)` for bash_call turn
   field**: not visible from the diff but presumably hashes the
   (line_offset, event_idx) tuple into a u32. The bash_call grammar
   is `bash_call:<session>:<turn>` where turn is u32 from F1.
   Concern: if turn is derived from line_offset/event_idx via a
   non-injective hash, two distinct bash invocations in one session
   could collide on the same turn. Verify the function — either it's
   bijective (e.g. event_idx alone if monotonic per session) or
   collisions are real.

2. **`EDITED_FILE` confidence is `Heuristic`** because chunk
   resolution at index time is best-effort against the current
   file content. But the anchor metadata stores
   `content_hash_at_edit` — that's the source of truth. P2's
   bbox_blame can use the anchor's content_hash to walk back through
   git blame to the historical chunk state. So the edge points at
   the CURRENT chunk that contains the edit's byte_range AT INDEX
   TIME. If the file has been edited after the tool call but before
   reindex, the current chunk may have moved. Flag for P2 to handle
   the "anchor → historical position via git blame" walk.

3. **Tool-call edges only emit when file is under a registered
   project.** If a session edits files outside registered projects
   (e.g. a one-shot edit to a sibling repo before registration),
   the provenance is lost forever. Operator workflow: when
   registering a new project, the reindex thread should retroactively
   walk transcripts and backfill edges. Document the gap; defer the
   backfill mechanism.

## Concerns

4. **`anchor_metadata` stores byte_range + content_hash + commit_sha
   in `Edge.metadata: BTreeMap<String, String>`.** Stringly-typed
   serialization. JSON-encoded. Works but typed access requires
   parse-on-read at every consumer. Consider a typed `EdgeMetadata`
   enum with variants for tool-call anchors, semantic-edge confidence,
   etc. Defer.

5. **Bash edges include `tool_call.input.command` in metadata**, but
   commands can be very long. Truncate to ~200 chars per the design
   §14.2 stdout_summary spec. Verify in `bash_metadata`; flag if
   missing.

6. **`tool_call_info` recognizes "Read", "Write", "Edit", "Bash"**
   plus lowercase variants. Codex/Claude use these names. Gemini
   hasn't been confirmed — if Gemini uses different names (e.g.
   `replace_in_file` instead of `Edit`), provenance is lost for
   Gemini sessions. Flag for verification when Gemini sessions
   become test cases.

## H2 fix observations

7. **H2 fix #1 (typed hybrid response)** — `HybridSearchResponse`
   exposed as pub(crate). discover_seed_entities consumes typed
   shape. JSON round-trip eliminated. Refactor saves ~70 LoC across
   both files. ✓

8. **H2 fix #2 (linear seed ranks)** — replaced O(n²)
   `seeds.iter().position(...)` with `.enumerate()`. ✓

9. **H2 fix #3 (reduce seed ref clutter)** — render_text now
   conditionally hides entity_ref when label differs from ref;
   shows ref only on fallback case. ✓

## Nits

10. **`tool_call_file_path`** falls back to `path` field for
    Codex-style tool calls. Reasonable; document the canonical
    field is `file_path` (Claude's name).

11. **The new `src/index/tool_edges.rs` module** is ~323 LoC. Self-
    contained; module boundaries are clean. Good factoring.

12. **`emit_event_edges` returns `Result<usize>`** with the count of
    edges emitted. The reindex thread can sum these for stats.
    Should it surface the count somewhere visible? `bbox_stats`
    could include "tool-call edges emitted in last reindex cycle."

13. **`crate::edge_index::append_edges` accepts a slice** but
    `emit_event_edges` always passes a single edge. Could call
    `append_edge` (singular) — but the slice API is more general.
    Subjective.

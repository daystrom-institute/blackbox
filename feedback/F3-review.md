# F3 + F1 fixes review

Commits `0f39f1a..ba0cc3c` (5 F1 fixes + F3).

## Issues (fix-forward)

1. **`Display for EntityRef` calls `render()` which can panic.**
   F1 fix #1 split `render` (panicking) from `try_render` (Result). The
   `impl fmt::Display for EntityRef` at line 301 still calls `render()`,
   so any formatter that prints an `EntityRef` constructed in-memory with
   an invalid provider (colon-bearing) will panic — including error
   messages, log lines, debug output. Display should never panic. Either:
   - Display calls `try_render` and falls back to a safe sentinel like
     `<invalid entity-ref: {field}={value}>`, OR
   - Construction APIs validate at build time so invalid in-memory state
     is unreachable.
   Pick one; document the chosen invariant.

2. **`git rev-list --max-parents=0 HEAD` is non-deterministic across
   clones for repos with multiple root commits.** Rare but possible
   (history-grafted repos, octopus-merged unrelated histories). Codex
   takes "first non-empty line"; different clones may iterate roots in
   different orders. Pick deterministically: lexicographic-min of the
   root SHAs, or walk the parent chain of HEAD all the way back. Add a
   test fixture with two root commits to exercise.

3. **Schema migration is in-process Rust, not workflow-driven.**
   `reset_index_on_schema_mismatch` runs synchronously inside
   `TranscriptIndex::open_or_create`. The shipped
   `schema-migration-arc.json` is a SPEC — every node sets a var to
   `true`, no actual drop/rebuild work. The design §12.6 implies the
   workflow IS the migration mechanism. Defensible reading: the workflow
   needs custom hook ops (`schema_migration_drop`, `schema_migration_rebuild`)
   that aren't in scope for F3, so F3 ships the in-process trigger + the
   workflow as a placeholder. Make this explicit in the release notes:
   "schema-migration-arc currently documents the migration shape;
   actual drop+rebuild runs in `TranscriptIndex::open_or_create`. A later
   phase wires hook ops so the workflow can drive the migration."

4. **`add_transcript_corpus_fields` only called from two doc-creation
   sites.** If any third doc-emission path exists (or is added later)
   without calling the helper, those docs miss `doc_type` +
   `parser_version`. The risk grows as later phases add `project_file` /
   `knowledge` / `commit` doc types, each with their own helpers. Either:
   - Make doc construction go through a single funnel (`build_doc(kind:
     DocKind, ...)`), OR
   - Add a debug_assert or test that scans tantivy after reindex and
     fails if any doc lacks `doc_type`.

## Concerns

5. **`#[allow(dead_code)]` on 9 of 11 new fields.** Honest signal that
   most fields are populated by later phases. Risk: the `#[allow]`
   attributes outlive their justification. Each phase that lights up a
   field should remove the attribute. Track in a checklist somewhere
   (release notes? per-phase done note?).

6. **`INDEX_SCHEMA_VERSION = "agentic-corpus-f3"` is phase-pinned.**
   Bumping at every schema change forces a re-walk every time. Cheap
   today (transcripts only); expensive once project_file + knowledge +
   commit indexing land (S2/S3/K1/G1). Worth thinking now whether
   schema versions should be more granular per-doc-type so adding
   commit ingestion doesn't force a transcript reindex.

7. **`design/agentic-corpus-release-notes.md` is a new artifact not
   in the impl skeleton.** Acceptable addition — useful place to note
   per-phase release behaviors. Make sure it stays updated in lockstep
   with later phases or it becomes stale-by-default.

## Nits

8. **`schema-migration-arc.json` `vars_schema` lists every step's
   "did it run" boolean.** Becomes noisy for longer arcs. The
   `bbox_thread` audit trail already records node completion via
   `done` notes; the per-node booleans add nothing. Consider dropping
   them unless a downstream consumer reads them.

9. **`continue_default` consequent in arc-budget packet is the literal
   string `"continue"`** — same as the classification name. Display is
   slightly confusing in audit output. Minor.

10. **Test in `reindex.rs` reconstructs the full schema by hand
    (`test_schema()`).** Drift potential: if `index/mod.rs` adds a new
    field and the test forgets to mirror it, the test passes (different
    schema instance) but production doc-emission might break later.
    Extract a `pub(crate) fn build_schema() -> (Schema, FieldHandles)`
    in `index/mod.rs` and have both production and test use it.

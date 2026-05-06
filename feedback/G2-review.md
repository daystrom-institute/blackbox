# G2 + P1 fixes review

Commits `8a590b6..cb3d109` (4 P1 fixes + G2 git notes serialization).

## Issues (fix-forward)

1. **`note_from_edges` derives `produced_by` from the FIRST edge only.**
   A single commit can have multiple sessions touching it (e.g. an
   ensemble dispatch where multiple specialists edit the same file
   pre-merge). The first edge's session may not represent the
   commit's full provenance. Either:
   - Aggregate sessions: list all distinct sessions in
     `produced_by.session_ids: Vec<String>` instead of singular.
   - Pick the most-recent session: sort edges by edit timestamp,
     use the latest.
   - Document explicitly that produced_by is "first session
     observed for this commit" with a code comment.

2. **`write_note` uses `git notes add -f`** which OVERWRITES existing
   notes for the commit. So calling `bbox_provenance_export` twice
   on the same corpus overwrites the prior note. For incremental
   exports this is acceptable (idempotent re-export). For
   collaborative scenarios where two machines write notes for the
   same commit (rare but possible if two machines edit through the
   same arc), the later export silently wins. Consider:
   - Use `git notes append` for additive semantics (operator merges
     via `notes.mergeStrategy union`).
   - Or document that single-machine ownership of provenance per
     commit is the expectation.

3. **`import_provenance_to_edges_dir` doesn't check for duplicate
   imports.** Each call appends edges to the sidecar; running
   `bbox_provenance_import` twice doubles the edges. The EdgeIndex
   `seen` HashSet at rebuild dedupes them, but the sidecar JSONL
   grows unboundedly. Add dedup at write time: skip if the edge
   (canonical hash) already exists in the sidecar.

## Concerns

4. **Registration-time replay always tries to import**, even for
   projects that aren't git repos or have no `refs/notes/bbox/*`
   ref. `import_provenance_to_edges_dir` returns 0 imported edges
   silently in that case. Acceptable; just verify it doesn't log
   warnings on every registration of a non-git or fresh project.

5. **`bbox_provenance_export` writes notes locally but doesn't push.**
   For cross-machine sync, operator must run `git push origin refs/notes/bbox/*`
   manually. Document the operator workflow in release notes:
   ```
   bbox_provenance_export → git push origin 'refs/notes/bbox/*' →
     remote machine: git fetch origin 'refs/notes/bbox/*:refs/notes/bbox/*' →
     bbox_provenance_import
   ```

6. **No bidirectional sync hook.** Operator must remember to
   export/import. A future improvement: post-commit hook that
   triggers export, post-fetch hook that triggers import. Defer.

## P1 fix observations

7. **P1 fix #1 (hash bash call turns)** — verify the hash is
   bijective per session OR collisions are documented. (Not
   inspected; flag if collision risk remains.)

8. **P1 fix #2 (document file edge confidence)** — code comment
   added explaining Heuristic confidence rationale. ✓

9. **P1 fix #3 (document unregistered project gap)** — release
   notes section added explaining the limitation + future backfill
   improvement. ✓

10. **P1 fix #4 (surface tool-call edge count)** — counter exposed.
    Verify which observability surface (bbox_stats vs
    bbox_embed_status vs bbox_inbox); look at the diff if uncertain.

## Nits

11. **`GitProvenanceNote` knowledge_writes field exists but is empty
    in v1** (M3's auto-digest hasn't shipped). Acceptable
    placeholder; will populate when M3 produces auto_apply
    knowledge writes.

12. **`notes_ref` formats as `refs/notes/bbox/<kind>`** with kind
    being a literal arg ("provenance" today). Other kinds
    anticipated (e.g. "knowledge"). Worth documenting that the
    namespace is open for kinds.

13. **`list_notes` parses `<note_sha> <commit_sha>` per line.**
    `git notes list` output format is documented; codex's parse is
    correct but brittle to git version changes. Pin via test
    fixture.

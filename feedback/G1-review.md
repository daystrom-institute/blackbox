# G1 + S4 fixes review

Commits `3ba191d..3b1fb87` (4 S4 fixes + G1 git ingestion).

## Issues (fix-forward)

1. **`COMMIT_BY_AUTHOR` uses `EntityRef::Brofile { name: format!("git:{author}") }`
   as the target.** The placeholder works mechanically but conflates
   git authors with brofile entities. Real brofiles are persona
   templates with backing JSON; `git:Mathieu Roy` is a synthesized
   ref that won't resolve through the brofile provider in D1. So
   `bbox_inspect_entity(brofile:git:Mathieu+Roy)` will 404. Either:
   - Add a virtual `git_author` entity type in F1's grammar (parallel
     to `task` and `bash_call`).
   - Drop the COMMIT_BY_AUTHOR edge entirely and just store the author
     in commit doc fields (already done — `commit.author_name` /
     `author_email` are in `GitCommit` but not surfaced as searchable
     fields on the tantivy doc).
   - Add new tantivy fields `commit_author_name` / `commit_author_email`
     and skip the placeholder edge.
   The shipped placeholder will leak as 404s once D1/D2 are live.

2. **`commit_log` uses `since..HEAD` range query without checking that
   `since` is an ancestor of HEAD.** If the user force-pushes (or
   rewrites history), the previously-recorded `last_ingested_sha`
   may no longer be reachable from the new HEAD. The range query
   then errors silently (returns empty) and the daemon thinks no
   commits need ingesting — but the new commits ARE there, just on a
   different ancestry chain. Add a check: if `git rev-parse <since>^{commit}`
   succeeds AND `git merge-base --is-ancestor <since> HEAD` returns 0,
   proceed; otherwise force-full re-ingestion of this project.

3. **`head_fingerprint` is the first 8 bytes of HEAD SHA hashed into
   a u64.** Used as `FileMeta.size` for change detection — clever
   reuse but conceptually overloaded. The "size" field semantically
   means file size; here it's a hash. Future maintainers will be
   confused. Either:
   - Add a dedicated `last_head_fingerprint: Option<u64>` field to
     FileMeta (migration concern).
   - Use a sentinel pattern: reserve `mtime = 0` to mean "git source"
     and document the contract.
   - Define a parallel `GitMeta` map separate from `FileMeta`.
   The current overloading works but earns a `// HACK:` comment at
   minimum.

## Concerns

4. **Author email and name are captured in `GitCommit` but not
   surfaced as searchable tantivy fields.** Only the message is
   indexed in `content`. So `bbox_search(query="Mathieu Roy commits")`
   wouldn't find anything via author. New tantivy fields
   `commit_author_name` and `commit_author_email` (STRING) would
   make author queries work — bumps schema version (acceptable;
   document in release notes).

5. **Embed enqueue for git_message** (`embed_queue::enqueue_git_message`)
   is the third route added to the queue stub (after knowledge in K1
   and the implicit project_file route from S2). Confirms that the
   per-route stub design is right. When E2 lands, three routes need
   wiring at once; flag for E2 prep.

6. **`changed_files_for_commit` uses `git diff-tree --root` for
   resolving touched files.** For merge commits with multiple
   parents, `--root` includes ALL changes against root, not against
   each parent. So merge commits emit lots of `COMMIT_TOUCHED_FILE`
   edges for files that weren't ACTUALLY changed in that merge.
   Either:
   - Use `--first-parent` for merge commits (loses second-parent
     changes).
   - Skip merge commits for touched-file edges entirely.
   - Accept the noise.
   Current behavior produces overly-broad edges on merge commits;
   noisy but not wrong.

7. **`indexed_commits` stat is incremented but never logged or
   reported anywhere visible.** `bbox_stats` (existing tool) doesn't
   include git ingestion stats. Add a stats line so operators can
   see how many commits got indexed per project on the last reindex
   cycle.

## S4 fix observations

8. **S4 fix #1 (transcript event collision documentation)** —
   comment landed at the IN_SESSION emission site. Acknowledges the
   `event_idx: 0` hardcoding and points at the schema-change path.
   ✓

9. **S4 fix #2 (warn on EdgeIndex materialization > 100k)** — added
   the size guard with a clear message. Operator visibility good. ✓

10. **S4 fix #3 (dedup scale doc)** — release notes updated. ✓

11. **S4 fix #4 (log skipped sidecar files)** — debug log added. ✓

## Nits

12. **`git_source_key(project_id)` returns `format!("git:{project_id}")`**
    used as a synthetic file_path on commit docs. Cute trick (so
    FileMeta tracking treats git history as a "file"); document in
    `git_source_key`'s doc comment.

13. **`commit.message` is stored verbatim including newlines.** A
    very long commit body (rare but possible) will produce a large
    tantivy doc. The existing 12KB chunk cap doesn't apply to
    commit docs since they're not chunked. Add a hard cap: truncate
    commit messages > ~16KB with a `[...]` suffix.

14. **`head_fingerprint` only hashes first 8 bytes of HEAD SHA.**
    If two HEADs differ only in bytes 8-39, the fingerprint
    collides → reindex thinks no change happened. SHA collisions in
    the first 8 bytes are astronomically unlikely (2^32 space) but
    deterministic for pathological cases. Use full SHA hash via
    `Hasher::hash_one` for u64. One-liner change.

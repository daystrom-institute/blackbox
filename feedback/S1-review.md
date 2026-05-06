# S1 + F3 fixes review

Commits `495e09d..b36f00a` (5 F3 fixes + S1 project registration).

## Issues (fix-forward)

1. **`bbox_project_register` and `bbox_project_list` MCP descriptions are
   too sparse.** They are one-liners ("Register a project directory for
   agentic-corpus indexing.") that miss the behavioral nudges the design
   §4.2 calls for. The agentic surface depends on the description layer
   to cue usage; starting bad here sets a precedent for D2/D3 (the real
   agentic tools). Each description should at minimum say:
   - What registration triggers (today: list-only; tomorrow: bootstrap arc)
   - Idempotency contract (re-registering same path returns existing record)
   - When `repo_id` is null vs populated
   - That `project_id` is per-machine; not portable
   Anti-pattern warning: "do not pass file paths" — already enforced at
   the API but worth stating.

2. **`register_path` calls `git_root_for_path` AND `repo_id_for_path` for
   git projects.** `repo_id_for_path` itself calls `git_root_for_path`
   internally — so 2 git invocations per registration. Cheap individually
   but every operation that touches projects pays it. Either refactor
   to expose `git_root_for_path` result through `repo_id_for_path` (e.g.
   `repo_id_for_path_with_root(canonical, git_root)`), or restructure
   `register_path` to compute `git_root` once and pass it down.

3. **`canonical_project_path` uses `anyhow::bail!`** for the file-vs-dir
   check, while `entity_ref::canonical_input_path` (which does the same
   check) uses `io::Error::new(InvalidInput, ...)`. Two error paths for
   the same invariant. Consolidate: either delete one of them and call
   the other (probably `canonical_project_path` calls `canonical_input_path`
   and propagates), or document why they differ.

## Concerns

4. **`repo_id` is computed at registration time and persisted, never
   refreshed.** If the user re-clones the same project (different first
   commit because of a graft, a new `--orphan` branch promoted to main,
   etc.), the persisted `repo_id` is stale until manually re-registered.
   Phase G2 (git notes serialization for cross-machine provenance)
   depends on `repo_id` matching across clones. Should `register_path`
   re-derive on each call when the path already exists? Or should
   `bbox_project_list` lazy-refresh? Defer the call; flag.

5. **`pub(crate) fn git_root_for_path`** in `entity_ref.rs` is now used
   by `projects.rs` (per the F3 fix expansion). Visibility creep is fine;
   just note that `entity_ref.rs` is becoming an "everything git" module.
   Consider extracting a `src/git.rs` module that owns git invocations
   when phase G1 lands (G1 is git ingestion; will need this code).

## F3 fix observations

6. **F3 fix #1 (non-panicking display)** — implementation is clean. The
   sentinel `<invalid entity-ref kind=session field=provider value=claude:code>`
   is grep-able and won't be confused with valid input. Captures both
   the field name and the offending value. Good.

7. **F3 fix #2 (deterministic root commit)** — extracted
   `git_first_commit_from_stdout` for testability. `sort_unstable` +
   first picks lexicographic min. Test with two roots verifies. Good.

8. **F3 fix #4 (doc emission funnel)** — `build_transcript_doc` is now
   the single entry point; both `event_to_doc_standalone` (test fixture)
   and `index_directory_standalone` (production) use it. The
   `project_fallback` parameter handles the case where the parsed event
   has no cwd; previously this was inlined per call site. Good.

9. **F3 fix #5 (shared schema builder)** — `pub(crate) fn build_schema`
   in `index/mod.rs`. Both production `open_or_create` and the reindex
   tests use it. Removed the duplicated `test_schema()`. Good.

## Nits

10. **`ProjectListResponse` exists as a wrapper struct just to JSON-serialize
    `{"projects": [...]}`.** Three lines for a struct that's used in one
    place. Inline as `serde_json::json!({"projects": ...})` and drop the
    type. Subjective.

11. **`save()` writes to `path.with_extension("json.tmp")`.** If the
    persisted path has no extension or a different one, this produces
    weird names. Use a `.tmp` suffix appended literally instead.

12. **`bbox_project_register` description omits the path argument
    spec** — it doesn't say "path: absolute path to project root" or
    similar. The Parameters struct exposes `path: String` but a CLI
    user reading the tool list won't know what shape of path to pass.
    Address with #1 above.

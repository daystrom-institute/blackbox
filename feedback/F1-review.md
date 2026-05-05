# F1 review — entity ref parser

Commit `ff08845`, `src/entity_ref.rs` (733 LoC).

## Issues (fix-forward)

1. **Round-trip property test doesn't cover provider-containing-colon.**
   `tests::random_entity` generates `provider: rng.token("p")` (no colons),
   but `session_id: format!("{}:{}", ...)` (intentional colon). So 10k cases
   exercise multi-colon session_id but never multi-colon provider. The parser
   uses `split_first` on `provider:rest` for `transcript`/`session` — a
   provider name with a colon would round-trip incorrectly. Add a generator
   variant that places colons in provider names; expect the round-trip to
   either preserve them via escaping or reject them at parse time. Pick one
   policy, write it down.

2. **`canonical_input_path` silently strips the filename when given a file.**
   `bbox_project_register("/path/to/file.txt")` resolves to a project_id for
   `/path/to/` rather than erroring. This masks programming mistakes —
   project registration should reject file paths, not silently treat them as
   directory paths. Either error explicitly or document the behavior.

3. **`hash_path` truncates to 4 bytes (8 hex chars).** Not documented.
   32-bit collision space; fine for one user's project set but the field
   width is invisible from the function signature. Add a doc comment, or
   move the truncation to a separate `truncate_hash` helper so the call
   site shows the choice.

4. **Silent fallback when `git` binary missing or not in PATH.**
   `git_root_for_path` returns `None` on `Command::new("git").output()`
   failure, indistinguishable from "this path isn't in a git repo." Both
   collapse to `repo_id == project_id`. At least log at warn-level on
   exec failure so missing-git is observable.

## Design-level concerns (NOT F1 bugs — spec defects exposed by F1)

5. **`repo_id` is per-machine because it's a realpath hash.** For projects
   under git, this breaks cross-machine portability of provenance — the
   killer feature G2 (`refs/notes/bbox/*`) needs commit refs to be stable
   across clones. Today: alice's daemon emits `commit:abcd1234:fffeeedd`,
   pushes notes; bob fetches notes; bob's daemon hashes his realpath as
   `bob1xyz0` and the replay can't match the edge target. Two fixes worth
   considering:
   - For git repos, derive `repo_id` from the first-commit SHA (or remote
     URL); fall back to realpath hash only when neither is available.
   - For git repos, normalize `repo_id` at replay time via a translation
     table keyed by remote URL.
   The first is cleaner. Either way, the design doc §5.6 + §6.2 + §15
   need to settle this before G1/G2 land. Surface as a `bbox_dispute` so
   we can decide before P2/G2 implementation hits the wall.

## Concerns (worth thinking about; not blockers)

6. **`#[serde(tag = "type", rename_all = "snake_case")]` on `EntityRef`
   sets a load-bearing JSON convention silently.** Any future tool that
   serializes/accepts an `EntityRef` as a JSON field is now bound to
   `{"type": "knowledge", "id": "..."}`. Design doc §6.2 only specifies
   the string grammar, not the JSON shape. If MCP tools accept string
   forms (per §6.3), this is fine; if any tool accepts the JSON form,
   write down the convention.

7. **`PARSER_VERSION` constant is unused.** Defined at line 11 but never
   referenced. F3 will wire it into tantivy schema versioning per the
   plan; until then it's a marker. Acceptable; just confirms phasing.

## Nits

8. **Empty-prefix input (`":abc"`) gives an unhelpful error.** Levenshtein
   to empty string finds nothing close enough, so the suggestion lists
   every type. Could special-case to "did you forget the type prefix?"

9. **`closest_type` levenshtein operates on bytes.** Unicode-incorrect.
   Type names are ASCII so it works today; flag if ever extended.

10. **`canonical_input_path` does no validation that the path exists** —
    `fs::canonicalize` will error on missing paths, propagating an
    `io::Error`. The error message won't say "use a path that exists,"
    just whatever the OS returns. Minor UX improvement opportunity.

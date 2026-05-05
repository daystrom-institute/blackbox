# F4 + F2a fixes review

Commits `6032b74..7dde53b` (3 F2a fixes + F4 catalog).

## Issues (fix-forward)

1. **Supersession is metadata-only; doesn't deactivate the underlying
   artifact.** `mark_superseded` flips `active=false` in the catalog but
   the workflow stays in `state.workflow_registry`, the packet stays
   compiled in the packet store, the brofile stays on disk. Operator
   superseding a packet expects it to STOP firing — it doesn't. Either:
   - Wire supersession to actually remove from the underlying store
     (`workflow_registry.remove(&name)`, packet `bbox_forget`,
     brofile delete), OR
   - Document explicitly that catalog supersession is observability-only
     and the operator must remove the underlying artifact separately.
   Pick one; the current behavior is a footgun.

2. **HTTP source download is unbounded.** `read_artifact_source` for
   `http://` or `https://` calls `reqwest::get(source)` with no size
   limit, timeout, or content-type check. A 10GB malicious response
   would happily load. Add: max body size (~1 MB), connect+read timeout
   (30s), content-type assertion (`application/json` / `text/plain`),
   and reject redirects to non-http(s) schemes.

3. **`discover_project_artifacts` directory-name convention is
   fragile.** Strips trailing `s` from the first path component
   (`workflows` → `workflow`, `packets` → `packet`). A project with
   `<project>/.bbox/workflow/` (singular) returns `kind="workflo"`. A
   project with `<project>/.bbox/data/` returns `kind="dat"`. Either:
   - Hardcode the expected directory names (`workflows`, `packets`,
     `brofiles`) and skip anything else, OR
   - Use an explicit map (`"workflows" → ArtifactKind::Workflow`).
   The fuzzy strip-`s` is a bug factory.

4. **Workflow capability validation is invoked through
   `BlackboxServer::new(state.clone())` in `install_artifact_value`.**
   Constructing a server just to call one method is awkward and pulls
   in side effects. Lift `validate_workflow_capabilities` to a free
   function or to a smaller type that doesn't carry the full server
   surface.

5. **`supersedes_chain` accumulation across N supersessions isn't
   tested.** The shipped test does v1 → v2 only. Add a v1 → v2 → v3
   case asserting `v3.supersedes_chain == ["v1", "v2"]`. The chain
   logic in `install_value` correctly extends from `prev_meta.supersedes_chain`
   so this should pass; the test confirms it.

## Concerns

6. **Catalog vs underlying store are two sources of truth.** Workflow
   installed via catalog lands in BOTH `~/.local/state/blackbox/artifacts/workflow/<name>.json`
   AND `state.store_dir.join("workflows")/<name>.json`. On restart, the
   workflow is loaded by the existing pre-F4 path (workflow_registry
   reads `store_dir/workflows`); the catalog is informational. If
   they ever diverge (manual edit to one but not the other), the
   catalog lies. Three options to consider:
   - Make the catalog the source of truth; deprecate the per-store
     install paths in the next phase.
   - Make the per-store paths the source of truth; catalog is a
     materialized view rebuilt at startup.
   - Add a startup consistency check that flags divergence to
     `bbox_inbox`.
   Defer decision but pin it before the catalog grows real users.

7. **`ArtifactKind::Packet` derives the catalog name from the `domain`
   field, not `name`.** Inconsistent with workflow/brofile (which use
   `name`). Defensible because packets are domain-keyed, but the
   asymmetry deserves a `///` doc comment on `artifact_name` so a
   future maintainer doesn't try to "fix" the inconsistency.

8. **`Brofile` install via `save_brofile(..., "global", ...)` hardcodes
   global scope.** The catalog has no project-scope concept. If users
   want per-project brofiles, the catalog can't express that today.
   Defer; flag.

## Nits

9. **`atomic_write_json` writes to `path.with_extension("tmp")`.**
   `with_extension` REPLACES the existing extension. So
   `metadata.json` becomes `metadata.tmp` — fine. But for the artifact
   file `arc-budget.json` it becomes `arc-budget.tmp` — also fine.
   The naming is a little misleading: the temp file is "metadata.tmp"
   not "metadata.json.tmp". Subjective.

10. **`F2a fix #1` (validate entity type hints) bundled a `pub(crate)`
    visibility change to `EntityType::from_prefix` in the same commit.**
    Acceptable because the fix needs the symbol exposed, but a separate
    `phase F2a fix: expose entity-type prefix lookup` commit would be
    auditable. Minor.

11. **`load_manifest`'s validation custom error includes the locator
    description**, which can be long. Truncate to ~80 chars in the
    error message to keep panic/log lines readable.

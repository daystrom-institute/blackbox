# AS-D1 review

Commit `cd39a84`.

## Issues

1. **`bbox_artifact_list(kind="agent")` returns superseded agents by default.**
   `design/agent-system-impl.md:49` says agent listing should be
   active-only by default, with `include_superseded=true` surfacing
   history. Current `ArtifactListParams` has no `include_superseded`
   field (`src/artifacts.rs:46-52`), and `ArtifactCatalog::list`
   pushes every metadata row without checking `meta.active`
   (`src/artifacts.rs:184-202`). The new tests assert the opposite
   behavior (`src/artifacts.rs:587-598`, `src/main.rs:8269-8292`).
   Fix-forward: add `include_superseded: bool` default false and update
   tests so default agent listing returns only active rows.

2. **Agent list entries do not include manifest summary.**
   AS-D1 components say list results include manifest summary:
   description, version, active flag (`design/agent-system-impl.md:39-40`).
   Current `ArtifactListEntry` only contains catalog metadata/path
   (`src/artifacts.rs:78-90`, populated at `src/artifacts.rs:191-202`).
   For agent kind, include at least `description` when present. Keep it
   optional and generic enough not to force AS-F1 schema yet.

## Nits

3. `src/main.rs:6544` comment says the minimal validation requires a
   name field, but that branch only checks object-ness. Name/version
   validation happens later in `ArtifactCatalog::install_value`. Make
   the comment match the code.

4. `agent_install_rejects_non_object` in `src/artifacts.rs:608-623`
   does not actually test object validation; it relies on name
   extraction failing. Either rename it to `agent_install_requires_name`
   or remove it in favor of the dispatch-layer non-object test.

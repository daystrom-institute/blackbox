---
title: "Project Taxonomy Standardization"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
tags:
  - corpus
  - projects
  - workspaces
  - containment
brief: "Separate durable project identity from concrete checkouts, container workspaces, cwd, and path transport details."
---

# Project Taxonomy Standardization

Status: proposed

## Thesis

Blackbox should stop using filesystem location as the overloaded identity for
project-scoped tools. A project selector such as `blackbox` or `d723917f`
should identify the durable corpus project regardless of whether the caller is
running on the host, inside a container mounted at `/work`, inside a transient
worktree, or in a future VM-backed sandbox.

Filesystem paths remain necessary, but they describe an execution view or a
specific checkout. They should not be the primary identity for corpus search,
knowledge, thread, artifact, or provenance queries.

## Problem

Several tool surfaces use names such as `project`, `project_dir`, `cwd`, `root`,
and `path` as though they were interchangeable. That works while every actor
sees the same host filesystem, but it breaks down once dispatch uses
containerized or VM-backed execution.

In a contained agent session, the useful path is short and stable:

```text
/work/crates/bro-harness/src/agent_loop.rs
```

The durable host checkout may be:

```text
/home/invidious/repos/transcript-search/.worktrees/task-123/crates/bro-harness/src/agent_loop.rs
```

Those two strings point at the same file for one execution session, but neither
string is the durable project identity. Persisting `/work` as a project key is
wrong; requiring the agent to use the host path loses the token and containment
benefits of a fixed workspace root.

## Vocabulary

Use these terms consistently:

| Term | Meaning | Lifetime | Example |
|---|---|---|---|
| `project` | Durable logical corpus identity | Stable across checkouts and containment | `blackbox`, `d723917f` |
| `repo` | Version-control identity backing one or more projects/checkouts | Stable for a git repository | `d4a1e491` |
| `checkout` | Concrete filesystem checkout on the daemon host | Exists until removed | `/home/.../transcript-search` |
| `worktree` | Concrete checkout created for one task or lane | Task/lane scoped | `/home/.../.worktrees/task-123` |
| `workspace` | Execution-local view of a checkout | Session scoped | `/work` |
| `cwd` | Current working directory inside a workspace or checkout | Command scoped | `/work/crates/bro-harness` |
| `path` | File or directory reference, preferably relative to the selected workspace/project | Request scoped | `crates/bro-harness/src/lib.rs` |

`workspace` is an alias layer. `project` is an identity layer.

One caveat the table glosses over: today's `project_id` is a hash of the
canonicalized host realpath (`entity_ref::project_id_for_path`,
`crates/bbox-corpus-core`). It survives `bbox_project_rename` only because the
registry preserves it in-place; a fresh registration of the same repo at a
different path — or on another host — mints a different id. So `project_id`
is host-scoped registry identity. Cross-host and cross-checkout durability
comes from `repo_id` (first-commit SHA) plus the alias layer below; the alias
is what lets two hosts refer to "the same project" without sharing a path
hash.

## Tool Semantics

Corpus and coordination tools should prefer logical project selectors:

```text
bbox_hybrid_search(project="blackbox", query="harness shell runner")
bbox_knowledge(project="blackbox", query="workspace project identifier")
bbox_thread_list(project="blackbox")
```

The daemon resolves project aliases and ids to a canonical `project_id`. The
caller does not need to know whether the active execution root is `/work`, a
host path, or a VM mount.

Filesystem-mutating and command-running tools need a concrete execution target:

```text
project = "blackbox"
workspace = <implicit session workspace>
cwd = "/work/crates/bro-harness"
path = "src/agent_loop.rs"
```

The daemon resolves this into a host checkout/worktree and rejects paths that
escape the selected workspace. The durable stores should record project ids,
repo ids, relative paths, and checkout/worktree ids where needed, not container
absolute paths.

## Resolution Order

When a tool receives project-like input, resolve it in this order:

1. Explicit `project_id`.
2. Registered project alias or slug, such as `blackbox`.
3. Host canonical project path.
4. Session workspace alias such as `/work`, translated through the active
   session mount table.
5. Current working directory, only as a fallback for legacy callers.

The result of resolution is a structured context:

```text
ProjectContext {
  project_id,
  repo_id,
  project_aliases,
  checkout_id?,
  host_root?,
  workspace_root?,
  path_map?
}
```

Tools that only search the corpus should stop after `project_id`. Tools that
touch files continue to `checkout_id` and `host_root`. Tools that speak to an
agent translate results back through `path_map`.

## Response Paths

Path rendering depends on audience:

- Agent-facing MCP responses should prefer execution-local paths such as
  `/work/crates/...` when the session has a workspace map.
- Operator-facing UI, provenance, and clickable links should prefer host paths
  or project-relative paths that can be mapped to host paths.
- Durable graph entities should prefer `project_id` plus relative path hashes
  and should not persist `/work` as an identity-bearing path.

This preserves the token savings of contained workspaces without making the
container mount point part of the corpus model.

## Existing Precedent

Much of this taxonomy already has partial code conformance; the migration is
consolidation, not greenfield:

- `ProjectRegistry::resolve` (`crates/bbox-indexing/src/projects.rs`) already
  accepts a `project_id` hex, a registered canonical path, or any absolute
  path (canonicalized). Aliases are the missing input form.
- Worktree→base mapping exists (`resolve_managed_fleet_worktree`) and is used
  by the store write-scope resolver (`src/tools/scope.rs`) and dispatch env
  resolution. It currently synthesizes a pseudo project record with a
  `:fleet-worktree` id suffix; `ProjectContext { project_id, checkout_id? }`
  should subsume that hack.
- Dispatch sessions already bind project implicitly: `AmbientContext`
  (`src/orchestration/mod.rs`) pins `cwd`/`project_dir` for worktree
  confinement and fills `default:mcp.*.project` from the canonical dispatch
  cwd.
- Durable entities already key on `project_id + rel_path_hash`
  (`EntityRef::ProjectFile`), and provenance git notes store relative paths
  plus `project_id` — both are already container-portable.
- `bbox_blame` already renders audience-aware dual paths
  (`BlameTarget { file_path, display_path }`).

The genuinely new slices are: aliases, the structured `ProjectContext` return
type, and the session workspace map. The workspace layer (`/work`, mount
tables, `path_map`) has no code today — no container, VM, or sandbox
execution exists — so resolution step 4 and agent-facing path translation
are forward-looking and must not block the resolver consolidation.

## Migration Shape

Additive migration is enough:

1. Consolidate the existing resolution logic into a shared project/workspace
   resolver used by `bbox_*`, `bro_*`, and workflow code that currently
   accepts `project`, `project_dir`, `cwd`, or path-like parameters.
2. Add alias support for registered projects so `project="blackbox"` and
   `project="d723917f"` resolve identically.
3. Attach an optional session workspace map to dispatched agents:

   ```text
   /work -> <host worktree root>
   /home/agent -> <host temp home>
   ```

4. Normalize inbound tool arguments at the boundary. Store structured project
   and path data internally.
5. Render outbound paths according to the caller audience.
6. Keep host-path and cwd fallback behavior for compatibility, but treat it as
   legacy inference rather than the preferred API.

## Non-goals

This design does not require immediate removal of existing `project_dir` or
`cwd` parameters. It defines the taxonomy new tools should use and the resolver
old tools should converge on.

It also does not require every project to have a globally unique human alias.
Aliases are convenience selectors over stable project ids. Ambiguous aliases
must fail closed and ask for an id or registered root.

## Resolved Questions

Resolved 2026-06-12 after grounding the proposal in the current code.

**Alias storage: superseded by catalog authority.** `.bbox/config.toml`
declares portable alias nominations. The durable project catalog owns accepted
selector aliases and changes them only through an epoch-checked catalog
transaction. Registration, publisher advance, and reload may report new
nominations but do not activate or revoke them. Migration preserves every
already-materialized registry alias as accepted state so selectors do not
regress. Conflicting nominations fail closed at acceptance and ambiguous active
aliases fail closed at resolution. This supersedes the earlier rule that
register/reindex materialized committed declarations automatically; see
`design/daemon-runtime/durable-project-catalog-impl.md` section 13.2.

**Checkout/worktree stay implicit in tool schemas.** The dispatch session
already binds the execution target (`AmbientContext` pins, structural worktree
detection, store redirects keyed on worktree paths); adding a checkout
selector to every file-touching tool would bloat the surface with no current
consumer. The resolver mints checkout identity internally from the canonical
path. Only cross-checkout admin tools (`bbox_project_rename`,
`bbox_project_eject`, and successors) take explicit paths, as they already do.
Revisit only when a concrete cross-checkout targeting consumer appears.

**Snapshot/branch selectors: deferred, hook reserved.** Indexing remains
single-checkout, current-HEAD. The designated extension point is
`EntityRef::ProjectFileV2.snapshot_id` (hash of `repo_id` + `project_id` +
HEAD SHA, behind `BBOX_PROJECT_REFS_V2`), which already exists in the ref
schema. A `snapshot=`/`rev=` query selector should be specified only when
multi-checkout indexing lands, and must key on that dimension.

**Dual path fields: yes — standardize the blame pattern.** `bbox_blame`
already ships `BlameTarget { file_path, display_path }`. New and migrating
tools adopt the same shape: an audience-rendered `display_path` alongside the
canonical form whenever the two differ (today, project-relative vs
host-absolute; later, workspace-mapped vs host). Lossless during migration
and precedented in code.

## Open Questions

None currently. Earlier open questions were resolved above.

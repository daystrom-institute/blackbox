---
title: "Storage and Performance Hygiene for Project-Derived State"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
  - storage
---

# Storage and Performance Hygiene for Project-Derived State

Date: 2026-05-13
Status: shipped; archived as the storage-performance design record

## Problem

Blackbox currently stores several different lifecycles in nearly the same
shape:

- durable, user-authored graph facts
- derived current-project facts
- derived git-history facts
- transcript/tool-call observations
- backup files created by compaction and recovery

The system can answer useful graph questions today, but the storage model makes
derived state look too much like durable truth. That is why project sidecars
grew into multi-gigabyte files and why `bbox_describe_schema` exposed surprising
entity counts. The immediate schema-count and compaction fixes reduce the
damage, but they do not fully fix the design.

The deeper issue: append-only storage is being used for facts that are actually
materialized views of a workspace snapshot.

## Current Shape

The important current identities are:

- `project_id`: hash of the canonical project path. This is already the
  workspace identity in practice.
- `repo_id`: hash of first commit, remote origin, or path fallback.
- `project_file` ref:
  `project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>`.
- `symbol` ref:
  `symbol:<project_id>:<qualified_name>:<defn_hash>`.

Project-file Tantivy documents are keyed by absolute `file_path` and filtered
by `project_id`. Incremental reindex deletes/replaces docs for changed paths
and purges docs whose source path disappeared. In effect, Tantivy already acts
like a current-workspace materialized view, but that lifecycle is implicit and
not mirrored cleanly in graph storage.

Graph sidecars are less clean:

- legacy project sidecars live at `edges/<project_id>.jsonl`;
- managed derived sidecars live at
  `edges/derived/<namespace>/<project_id>.jsonl`;
- explicit/tool edges can still append into project sidecars;
- compaction removes legacy `Derived` edges but keeps explicit and malformed
  lines, writing timestamped `.bak-*` files.

Recent fixes added:

- entity type counts directly over `EdgeIndex` keys, avoiding `known_refs`
  clone/sort;
- filtering of sidecars by registered project ids during rebuild;
- full-refresh replacement for managed project/git derived edges;
- legacy sidecar compaction after full project refresh;
- daily background full refresh by default.

Those were the first hygiene cuts. The implementation now follows through with
lifecycle-owned storage, bounded GC, manifest-index active loading, snapshot
materialization, dirty overlays, legacy extraction, and v2 entity refs.

## Failure Modes

### Branch Switches

When a user changes branches, the working tree becomes a different snapshot.
Tantivy file docs mostly converge because changed/deleted absolute paths are
handled by reindex. Graph facts are weaker:

- file chunks from branch A and branch B share one `project_id`;
- `project_file` refs do not include `HEAD`, branch, or snapshot id;
- append-only derived edges can leave old `CONTAINS_SYMBOL`,
  `DEFINED_IN`, `NEXT_SECTION`, and git linkage facts visible until replaced or
  compacted;
- git ingest metadata is keyed by `project_id`, so branch changes and unrelated
  heads are handled procedurally, not represented as first-class state.

The system therefore conflates "currently true in this checkout" with "was
observed at some point in this project".

### Worktrees

Git worktrees expose the opposite identity problem. Two checkouts of the same
repo have different canonical paths, so direct registration creates different
`project_id`s even when they share one `repo_id`.

That is sometimes useful: each worktree has independent current files and
branch state. But the model does not name that distinction explicitly. A
worktree is treated like a separate project with an optional shared repo id,
instead of a workspace instance of a repo.

Nested `.worktrees` are skipped by project-file scanning, which avoids indexing
the same repo accidentally under a parent checkout. External worktrees still
need explicit registration.

### Backups and Orphans

Compaction backups are operational recovery files. They are not part of the
semantic model, but today they live beside active sidecars and need manual
pruning unless an operator notices growth.

Unregistered project sidecars are now ignored during rebuild, but they can
still consume disk. This is better than loading stale truth, but it still leaves
opaque storage behind.

### Count Semantics

`bbox_describe_schema` should describe active graph state. It should not count
orphaned projects, stale snapshots, inactive branch views, or backup payloads.
The storage model should make that a natural property, not a filter patched onto
rebuild.

## Design Thesis

Separate storage by lifecycle.

Append-only logs are appropriate for facts whose history matters. Replaceable
materialized views are appropriate for facts that describe current workspace
state. Cache entries need manifests and retention policy. Backups need bounded
retention.

The daemon, not the user, should own garbage collection.

## Target Model

Use four top-level lanes under the Blackbox state directory:

```text
edges/
  explicit/
    project/<project_id>.jsonl
    repo/<repo_id>.jsonl
    global.jsonl

  observed/
    transcript/<provider>/<session_id>.jsonl
    tool/<provider>/<session_id>.jsonl

  materialized/
    workspace/<project_id>/<snapshot_id>/
      manifest.json
      project.jsonl
      symbols.jsonl

    repo/<repo_id>/
      commits.jsonl

  backups/
    <yyyy-mm-dd>/<source-kind>/<id>/<file>.jsonl
```

This layout is conceptual; the exact path names can change. The required
property is that lifecycle is visible in the path.

### Explicit Edges

Explicit edges are durable facts intentionally authored by a user, agent, or
tool. They are append-only by default and carry provenance. They survive branch
changes and worktree deletion unless explicitly forgotten.

Examples:

- semantic links from a transcript to a file edit;
- a user-authored `DESCRIBES` edge;
- a durable note-to-thread relation;
- a manually approved knowledge relation.

### Observed Edges

Observed edges are event history. They can be append-only, but they should not
pretend to be current state.

Examples:

- "session S edited path P at commit C";
- "tool call T referenced file F";
- "bro B emitted task result R".

Observed edges are useful for provenance and blame. They should be queryable as
history, but not automatically folded into current schema counts unless the
caller asks for historical mode.

This does not require a new `EdgeProvenance` variant. Current `Tool`
provenance maps naturally into the observed lane. The observed lane is the
storage/query lifecycle for event-derived facts.

### Materialized Edges

Materialized edges describe a computed view of a workspace snapshot. They are
replaceable, content-addressed or snapshot-addressed, and safe to regenerate.

Examples:

- project chunk adjacency;
- `CONTAINS_SYMBOL`;
- `DEFINED_IN`;
- current-file-to-current-commit linkage;
- immutable commit graph projection for a repo.

Materialized files are never append-only. A refresh writes a new snapshot or
replaces the current snapshot atomically. Old snapshots are cache entries.

There are two subtypes:

- **workspace materializations** are snapshot-scoped and describe the files and
  symbols visible in one worktree at one head/dirty fingerprint;
- **repo materializations** are repo-scoped and describe immutable commit facts
  keyed by `repo_id`.

Commit docs and commit-to-parent edges use the repo-scoped subtype when repo
materialization is available. Edges from current chunks to commits remain
workspace snapshot-scoped, because they depend on which files/chunks exist in
that checkout.

## Identity Model

Make the existing workspace identity explicit:

```text
repo_id     stable repository identity
project_id  stable canonical path / worktree identity
snapshot_id repo/project/indexer/chunker/head identity
```

Do not add a second `workspace_id` field that is merely an alias for
`project_id`. That would make the data model look more precise without changing
behavior. In this design, `project_id` remains the public compatibility id and
is documented as "workspace id" semantically. Snapshot-specific precision is
provided by the v2 ref types.

Implemented manifest fields:

```json
{
  "project_id": "existing path hash; workspace identity",
  "repo_id": "stable repo identity when available",
  "canonical_path": "/home/user/repos/project",
  "git_common_dir": "/home/user/repos/project/.git or common dir",
  "git_worktree_dir": "...",
  "branch": "main",
  "head_sha": "abcdef...",
  "dirty": false,
  "dirty_fingerprint": "optional invalidation hash, not snapshot identity",
  "indexer_version": "project-index-vN",
  "chunker_version": "chunker-vN",
  "active_snapshot_id": "...",
  "active_dirty_overlay_id": "dirty-current"
}
```

The important change is that repo state and workspace state become separate,
and branch/head state becomes a snapshot property.

### Snapshot ID

For git repositories:

```text
snapshot_id = hash(repo_id, project_id, head_sha, indexer_version,
                   chunker_version)
```

For non-git directories:

```text
snapshot_id = hash(project_id, source_tree_fingerprint,
                   indexer_version, chunker_version)
```

Dirty worktrees are allowed, but they should not create an unbounded stream of
content-addressed snapshots. Dirty state should be a single replaceable overlay
per workspace:

```text
materialized/workspace/<project_id>/dirty-current/
```

The dirty fingerprint is an invalidation/check field for that overlay, not part
of `snapshot_id`. The overlay is rewritten when dirty state changes and is
dropped when the workspace returns to a clean `HEAD`.

The dirty fingerprint uses a coarse-to-precise ladder:

1. `git status --porcelain=v1 -z` hash plus tracked file mtimes/sizes;
2. content hash for changed files only;
3. full tree hash when the caller needs maximum precision.

Querying an active dirty workspace reads the clean `HEAD` snapshot plus the
dirty overlay, with overlay facts winning for files it covers. This avoids
snapshot churn while still making dirty state visible.

Overlay merge granularity is per file. If a dirty overlay contains any
materialized facts for a relative path, those facts replace the clean snapshot's
workspace materialized facts for that path. Unchanged paths continue to read
from the clean snapshot. Per-file replacement is the supported merge unit
because chunk boundaries can change between clean and dirty states.

## Query Semantics

Default graph queries should read:

- explicit edges according to edge-kind scope;
- observed edges only when provenance/history is requested;
- the active clean materialized snapshot for each registered workspace;
- the active dirty overlay for dirty workspaces;
- repo materializations for repos referenced by active workspaces.

Historical mode can opt into:

- inactive snapshots;
- all snapshots for a repo;
- observed event lanes;
- compacted backup inspection, if exposed at all.

`bbox_describe_schema` should default to active graph state:

```text
schema_counts = explicit active edges
              + active materialized snapshots
              + active repo materializations
              + live task/thread/note/knowledge stores
```

It should also report storage hygiene separately:

```json
{
  "active_bytes": 123456,
  "cache_bytes": 456789,
  "backup_bytes": 987654,
  "orphan_bytes": 1234,
  "inactive_snapshots": 12,
  "prunable_bytes": 345678
}
```

The caller should not need to infer storage problems from weird entity counts.

### Edge-Kind Scope

Lifecycle alone is not enough; edge kind also matters.

```text
CONTAINS_SYMBOL       workspace snapshot
DEFINED_IN            workspace snapshot
NEXT_SECTION          workspace snapshot
EDITED_FILE           observed history
EDITED_BY_SESSION     observed projection
DESCRIBES             explicit cross-snapshot fact
DERIVED_FROM          explicit or observed, depending on producer
SUPERSEDES            explicit catalog/history fact
```

The loader should use an edge-kind policy table rather than assuming every edge
in a lane has the same active-query behavior. This prevents `DESCRIBES`-style
durable links from being lost when project snapshots rotate, while still
keeping `CONTAINS_SYMBOL` tied to the active workspace view.

## Retention and GC

Add daemon-owned retention policy:

```json
{
  "materialized_snapshots": {
    "keep_active": true,
    "keep_recent_per_workspace": 3,
    "keep_recent_per_repo": 10,
    "branch_switch_grace_minutes": 60,
    "max_age_days": 14
  },
  "dirty_overlays": {
    "keep_active_only": true
  },
  "observed": {
    "default": "keep",
    "max_total_bytes": null,
    "phase_3_policy_gate": true
  },
  "backups": {
    "keep_newest_per_source": 1,
    "max_age_days": 7,
    "max_total_bytes": 2147483648
  },
  "orphans": {
    "ignore_in_queries": true,
    "auto_prune_after_days": 30
  }
}
```

GC should run as part of the daemon's background maintenance loop and expose a
dry-run/apply tool. The dry-run output must be exact: paths, bytes, reason, and
retention rule.

GC does not synchronously delete old snapshots during a branch switch. Branch
hopping is common during development, and immediate deletion defeats cache
reuse. A branch switch only marks the previous snapshot inactive and
eligible after `branch_switch_grace_minutes`.

Manual pruning remains possible, but it becomes an operator override rather
than the normal path.

Observed history defaults to retention because it is the provenance substrate.
That does not mean it can be ignored operationally: storage health must report
observed bytes separately, and Phase 3 must either set an explicit cap/window or
record a deliberate no-cap decision before snapshot storage ships.

### Orphan Definition

"Orphan" must be precise:

- `unregistered_active_candidate`: sidecar or snapshot whose id is not in the
  current project registry, but whose canonical path still exists in a
  manifest;
- `explicitly_unregistered`: storage for a project the user removed through a
  Blackbox unregister operation;
- `dangling_path`: storage whose canonical path no longer exists;
- `legacy_unknown`: legacy sidecar with no manifest and no registered project
  match.

Default GC should ignore all orphan classes in active queries. It should only
auto-prune `dangling_path` and `legacy_unknown` after the grace period. Pruning
`explicitly_unregistered` should be a separate apply decision, because explicit
unregistration may be reversible or accidental.

## Branch and Worktree Behavior

### Branch Switch

When the daemon observes a registered workspace whose `head_sha` or dirty
fingerprint changed:

1. compute the clean-HEAD candidate snapshot id;
2. if the clean snapshot already exists, mark it active;
3. otherwise rebuild materialized project/git edges into that clean snapshot;
4. if the worktree is dirty, rebuild `dirty-current` as a replaceable overlay;
5. if the worktree is clean, remove any stale dirty overlay for the workspace;
6. atomically update the workspace manifest's `active_snapshot_id`, dirty flag,
   and overlay metadata;
7. schedule retention-policy GC for inactive snapshots.

No legacy append sidecar should be touched for derived current-state edges.

Tantivy should follow the same rule at the document level. The default search
index can remain current-workspace only: branch switch deletes/replaces docs for
the active workspace as it does today. Historical snapshot search is a separate
mode and should not be faked by leaving old project-file docs in the default
index.

### Startup and Rebuild Cost

Snapshot directories must not make startup slower by replacing one glob with a
deeper glob. The daemon should maintain a small manifest index:

```text
materialized/manifest-index.json
```

The manifest index maps registered `project_id`s to active snapshot paths,
dirty overlay paths, and repo materialization paths. Startup loads this index,
then verifies only the active paths. If the index is missing or corrupt, the
daemon can rebuild it by scanning manifests as a repair path, not as the normal
hot path.

The edge-sidecar watcher should watch the manifest index plus active paths. It
should not treat every inactive snapshot file as a reason to rebuild the active
`EdgeIndex`.

### External Worktree

When a user registers an external worktree:

1. derive the same `repo_id` as the main checkout;
2. derive a different `project_id`;
3. store a workspace manifest pointing at the shared repo id;
4. materialize snapshots independently for that workspace;
5. share repo-level immutable commit docs/edges where possible.

The UI/API can then say:

```text
repo: blackbox
workspaces:
  - /home/user/repos/transcript-search        main @ abc123
  - /home/user/repos/transcript-search-fix    feature/x @ def456
```

That is the model users expect.

## Migration Plan

### Phase 1: Make Existing Hygiene Explicit

- Keep current files readable.
- Keep `project_id` as the public compatibility id.
- Add storage-health reporting:
  active sidecars, managed sidecars, backups, orphan sidecars, bytes, lines.
- Add a `bbox_storage_gc(dry_run=true)` tool with exact deletion candidates.
- Add retention config with conservative defaults.

### Phase 2: Stop Appending Derived Current-State Edges

- Route all project/git derived edges through managed replaceable sidecars.
- Leave legacy sidecars for explicit/user-authored edges only.
- Add tests that branch-like refreshes do not increase derived line counts.
- Make full refresh compaction unnecessary for normal operation.
- Audit every caller of `append_project_edges` and classify it as `explicit`,
  `observed`, or `materialized`. Materialized callers must move to
  replace/write-snapshot APIs before Phase 2 is considered done.

### Phase 3: Add Workspace Manifests

- Add a workspace manifest for each `project_id`; do not introduce a duplicate
  `workspace_id` alias yet.
- Store `repo_id`, branch, head, dirty fingerprint, and active snapshot id.
- Make sidecar loading read active manifests instead of globbing `*.jsonl`.
- Add `materialized/manifest-index.json` so active-path resolution is O(number
  of registered workspaces), not O(number of cached snapshots).
- Decide observed-history retention explicitly: bounded cap/window, archival
  compression, or deliberate no-cap with storage-health alerting.

### Phase 4: Snapshot Materialized Edges

- Write materialized edges under snapshot directories.
- Switch active snapshot by manifest update.
- Keep a small bounded cache of inactive snapshots.
- Add repo/workspace listing commands that expose active/inactive snapshot
  counts and prunable bytes.
- Split immutable repo commit materialization from workspace-specific
  current-chunk-to-commit materialization.

### Phase 5: Tighten Entity Refs

Snapshot-specific refs are implemented as explicit v2 types:

```text
project_file_v2:<project_id>:<snapshot_id>:<rel_path_hash>:<chunk_hash>:<idx>
symbol_v2:<project_id>:<snapshot_id>:<qualified_name>:<defn_hash>
```

New project indexing emits these refs when `BBOX_PROJECT_REFS_V2=1`. Old refs
continue to parse and resolve through the active snapshot. The explicit `_v2`
type is noisier than overloading segment counts, but safer for stored
knowledge, notes, git notes, and transcripts that already contain old refs.

## Compatibility

Existing refs must continue to parse. The migration can resolve old refs by:

- interpreting old `project_id` as the workspace id;
- searching the active snapshot first;
- falling back to recent inactive snapshots only when explicitly requested;
- preserving old sidecar readers until legacy files are empty or archived.

This avoids breaking stored transcripts, notes, and knowledge entries that
already cite `project_file:<project_id>:...`.

### Legacy Extraction

The migration needs a one-time extraction step:

1. read each legacy `edges/<project_id>.jsonl`;
2. drop `Derived` edges only after their managed replacement exists;
3. move `Tool` provenance edges into `observed/`;
4. move `Explicit` provenance edges into `explicit/`;
5. retain malformed lines in a quarantine file with path and line number;
6. write a migration manifest recording source path, counts, hashes, and backup
   path.

Compaction alone is not enough. It reduces disk use, but it does not establish
lane ownership.

## Operational Invariants

The final system should maintain these invariants:

1. `bbox_describe_schema` active counts never include backups.
2. Active counts never include unregistered or inactive workspaces.
3. Derived current-state edges are replaceable, not append-only.
4. Branch switches do not require user cleanup.
5. External worktrees share repo identity but keep independent workspace
   snapshots.
6. GC has exact dry-run output and bounded default retention.
7. Explicit edges and observed provenance are not destroyed by project refresh.
8. All storage lanes have visible ownership and lifecycle.
9. Dirty worktrees use one replaceable overlay per workspace, not one snapshot
   per save.
10. Startup loads active manifests, not every cached snapshot.

## Resolved Policy

- Commit docs are shared by `repo_id` when repo materialization exists; current
  workspace chunk-to-commit facts remain workspace-scoped.
- Explicit project edges are stored by `project_id`; repo-derived facts use
  `repo_id` materialization only where the fact is truly workspace-independent.
- Snapshot retention is policy-driven GC, with active snapshots and dirty
  overlays protected. Long-running investigations should use retained snapshot
  ids in v2 refs rather than mutating the active workspace.
- Backups use bounded retention rather than mandatory compression.
- Observed event history is retained as provenance substrate. Storage health
  reports observed bytes so operators can see growth.

## Implemented Sequence

The implementation followed the intended order: make lifecycle visible before
changing identity, then move derived state to replaceable materializations, then
add manifests/snapshots and compatibility-preserving v2 refs.

1. add storage health and GC dry-run/apply;
2. finish moving derived edges to managed replaceable sidecars;
3. add workspace manifests;
4. only then introduce snapshot directories.

The result is no opaque disk growth, no manual backup pruning for normal
operation, and no stale branch/worktree facts leaking into active graph
answers.

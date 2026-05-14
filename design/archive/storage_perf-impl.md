# Storage and Performance Hygiene — Implementation Record

Date: 2026-05-13
Companion to: `design/archive/storage_perf.md` (architecture).
Status: shipped; archived as the implementation record

This record maps the shipped storage-performance work back to the incremental
cuts that landed it. The order was intentional: first make storage visible and
pruneable, then stop new append-only damage, then introduce manifests,
snapshots, legacy extraction, and v2 refs.

## Completion Status

All phase gates in this record are implemented:

| Phase | Landed surface |
|---|---|
| 1 — storage health and GC | `bbox_storage_health`, `bbox_storage_gc`, backup/temp/orphan/inactive snapshot inventory, exact dry-run/apply candidates. |
| 2 — lane split | lifecycle-specific edge write APIs, materialized replacement writers, observed and explicit append APIs, writer audit in `src/edge_index.rs`. |
| 3 — manifests and active loader | workspace manifests, `manifest-index.json`, active-path loading with fallback classification, observed retention policy. |
| 4 — snapshots and dirty overlay | clean snapshot ids, snapshot directories, dirty-current overlay, branch/worktree behavior, snapshot GC protection. |
| 5 — legacy extraction | `bbox_storage_migrate_legacy_edges`, extraction planning, explicit/observed lane install, quarantine, idempotent migration behavior. |
| 6 — v2 refs | `project_file_v2` and `symbol_v2` parser/provider support, exact snapshot refs, feature-gated producer emission. |

```
Phase 1 ──▶ Phase 2 ──▶ Phase 3 ──▶ Phase 4 ──▶ Phase 5 ──▶ Phase 6
   │           │           │           │           │
   │           │           │           │           └── v2 refs behind emission flag
   │           │           │           └── dirty overlay + branch behavior
   │           │           └── manifest-index loader
   │           └── lane split + derived append audit
   └── storage health + GC dry-run/apply
```

## Non-Goals

- Do not replace Tantivy or move to a graph database.
- Do not break existing `project_file:<project_id>:...` refs.
- Do not delete explicit/user-authored edges during derived-state cleanup.
- Do not make branch switches require user commands.
- Do not load cached/inactive snapshots into default active graph queries.

## Phase 1: Storage Health and GC Surface

**Prerequisites:** current code after the schema-count/compaction fixes.

**Goal:** make disk growth visible and pruneable without changing graph
semantics.

### 1.1 Storage Inventory

Add a storage inventory module, likely `src/storage_health.rs` or
`src/edge_index/storage.rs`, that scans:

- active legacy sidecars: `edges/<project_id>.jsonl`;
- managed derived sidecars: `edges/derived/<namespace>/<project_id>.jsonl`;
- compaction backups: `*.bak-*`;
- compact temp files: `*.compact-*`;
- unregistered project sidecars;
- malformed/quarantine files once Phase 2 exists.

Input state:

- registered projects from `ProjectRegistry`;
- edge directory from `edge_index::edges_dir_from_bro_store` /
  `edges_dir_from_projects_path`.

Output shape:

```json
{
  "active_bytes": 123,
  "managed_derived_bytes": 456,
  "backup_bytes": 789,
  "orphan_bytes": 12,
  "temp_bytes": 34,
  "prunable_bytes": 56,
  "files": [
    {
      "path": "...",
      "kind": "backup | legacy | managed_derived | orphan | temp",
      "project_id": "d723917f",
      "bytes": 123,
      "reason": "newest backup retained | older-than-newest backup"
    }
  ]
}
```

### 1.2 MCP Tool: `bbox_storage_health`

Expose a read-only tool:

```text
bbox_storage_health(project?: string, include_files?: bool)
```

Default response should be compact: totals and top offenders. `include_files`
adds exact file rows.

### 1.3 MCP Tool: `bbox_storage_gc`

Expose an apply-gated tool:

```text
bbox_storage_gc(
  dry_run: bool = true,
  project?: string,
  prune_backups: bool = true,
  prune_orphans: bool = false,
  prune_temps: bool = true,
  max_backup_age_days?: u64,
  keep_newest_backup_per_source: u64 = 1
)
```

Rules:

- `dry_run=true` is the default.
- `dry_run=false` deletes only paths returned by the same rule engine.
- The response must list every path, byte count, and rule.
- Never delete active `.jsonl` sidecars in Phase 1.
- Never prune `explicitly_unregistered` storage until Phase 3 can classify it.
- Delete compact temp files only when they are older than a conservative grace
  period, e.g. 24 hours.

### 1.4 Tool Docs and Tests

Update `src/tool_docs.rs` for both tools.

Tests:

- inventory classifies active sidecars, backups, managed sidecars, temps;
- GC dry-run reports exact candidates without deleting;
- GC apply deletes only candidates from the dry-run rule;
- newest backup per source is retained;
- unregistered sidecars are reported but not deleted by default.

**Acceptance gate:** an operator can run health and GC dry-run and see the same
kind of backup/orphan picture we inspected manually, without shelling out.

**Rollback:** remove the new tools/module. No data migration happens in this
phase.

## Phase 2: Lane Split and Derived Append Audit

**Prerequisites:** Phase 1.

**Goal:** stop new derived current-state facts from entering legacy append
sidecars.

### 2.1 Classify Existing Writers

Audit every caller of:

- `append_project_edges`;
- `append_edges`;
- `append_edges_dedup`;
- `replace_project_edges`.

Classify each caller:

```text
materialized  computed current workspace/repo view
observed      event/provenance history, usually Tool provenance
explicit      user/agent-authored durable fact
global        non-project graph support, e.g. catalog/agent edges
```

Expected initial classification:

- `src/index/project_files.rs`: materialized workspace edges.
- `src/index/git_history.rs`: split; commit facts are repo materialized,
  current-chunk-to-commit facts are workspace materialized.
- `src/index/tool_edges.rs`: observed tool/session history.
- semantic edge write paths in tests/tools: explicit unless provenance says
  `Tool`.
- agent/catalog provenance in `src/server/routes.rs`: global/explicit or
  observed depending on producer.

Record the audit in this implementation doc or a short table near the new
storage module so it does not rot.

### 2.2 Write APIs by Lifecycle

Add lifecycle-specific write APIs in `src/edge_index.rs`:

```rust
append_explicit_edges(edges_dir, scope, id, edges)
append_observed_edges(edges_dir, source, id, edges)
replace_materialized_edges(edges_dir, namespace, id, edges)
```

At first these can wrap existing paths:

- explicit -> legacy `edges/<project_id>.jsonl` or `edges/explicit/...`;
- observed -> legacy `edges/<project_id>.jsonl` or `edges/observed/...`;
- materialized -> existing `edges/derived/<namespace>/<project_id>.jsonl`.

The point is to make call sites declare intent before paths change.

### 2.3 Move Materialized Callers

Change materialized callers to use `replace_materialized_edges`:

- project chunk/symbol edges: namespace `project`;
- git current chunk linkage: namespace `git-current`.

Phase 2 deliberately chooses full namespace replacement over partial
incremental materialized writes. The incremental reindex path may skip work when
its input fingerprint is unchanged, but if it writes `project` or `git-current`
it rebuilds and replaces the complete namespace for the current checkout. It
must not append only the changed chunks.

Implementation notes:

- add a stable input fingerprint per materialized namespace so the 120s
  reindex tick can skip unchanged projects;
- keep `force_git_full` as an implementation detail, not as the lifecycle
  contract;
- reject `Derived` edges in append-only explicit/observed APIs in debug builds
  and tests;
- move immutable commit facts only when they can be written to a repo-scoped,
  content-addressed lane keyed by commit SHA. Until that exists, leave commit
  history out of the Phase 2 materialized replacement acceptance gate instead
  of pretending it is a workspace current-state fact.

The write API must never append workspace materialized current-state edges.

### 2.4 Legacy Extraction Dry-Run

Add a dry-run function:

```rust
plan_legacy_edge_extraction(edges_dir, project_id) -> LegacyExtractionPlan
```

It reads `edges/<project_id>.jsonl` and counts:

- `Derived` lines that can be dropped after managed replacement exists;
- `Tool` lines that should move to observed lane;
- `Explicit` lines that should move to explicit lane;
- malformed lines that should move to quarantine;
- blank lines.

Do not apply extraction yet unless Phase 2 tests prove all materialized writers
have moved.

### 2.5 Tests

Tests:

- branch-like reindex does not increase materialized sidecar line count;
- `append_project_edges` is no longer used by materialized project/git code;
- extraction dry-run classifies `Derived`, `Tool`, `Explicit`, malformed, blank;
- `bbox_describe_schema` active counts remain unchanged by backup files;
- old legacy sidecars still load for compatibility.

**Acceptance gate:** after repeated reindex/branch-like refresh, derived
materialized sidecars are bounded and replaceable; only explicit/observed lanes
append.

**Rollback:** before call sites are switched, lifecycle APIs can route back to
old paths. After materialized callers move to replacement semantics, rollback
is a code revert plus materialized reindex. No ref format or durable data
migration happens in this phase.

## Phase 3: Workspace Manifests and Active Loader

**Prerequisites:** Phase 2.

**Goal:** stop active graph loading from globbing arbitrary sidecars.

### 3.1 Workspace Manifest

Add a manifest file per registered project:

```text
edges/materialized/workspace/<project_id>/manifest.json
```

Schema:

```json
{
  "version": 1,
  "project_id": "d723917f",
  "repo_id": "...",
  "canonical_path": "/home/invidious/repos/transcript-search",
  "git_common_dir": "...",
  "git_worktree_dir": "...",
  "branch": "main",
  "head_sha": "...",
  "dirty": false,
  "dirty_fingerprint": null,
  "indexer_version": "project-index-v1",
  "chunker_version": "chunker-v1",
  "active_snapshot_id": "head-...",
  "active_dirty_overlay_id": null,
  "updated_at": "2026-05-13T..."
}
```

Keep `ProjectRecord` backwards compatible. It can remain small while the richer
state lives in manifests. Add fields to `ProjectRecord` only if a query/tool
needs them.

### 3.2 Manifest Index

Add:

```text
edges/materialized/manifest-index.json
edges/materialized/manifest-index.lock
```

Schema:

```json
{
  "version": 1,
  "workspaces": {
    "d723917f": {
      "manifest": "workspace/d723917f/manifest.json",
      "active_snapshot": "workspace/d723917f/snapshots/<id>",
      "dirty_overlay": null,
      "repo_materialization": "repo/<repo_id>"
    }
  },
  "updated_at": "..."
}
```

Update it under a single writer lock:

1. acquire `manifest-index.lock` with an exclusive advisory lock or
   create-new lockfile fallback;
2. reread the current manifest index after acquiring the lock;
3. validate referenced workspace manifests still exist;
4. write `<manifest-index.json>.tmp-<pid>`;
5. fsync the temp file and parent directory where supported;
6. rename over `manifest-index.json`;
7. release the lock.

All writers that update a workspace manifest and the index use this lock:
foreground reindex, background reindex, storage migration, manifest repair, and
GC. Loaders do not take the lock; they validate paths and fall back if they see
a partial or stale view.

### 3.3 Active Loader

Change `EdgeIndex::rebuild` sidecar loading:

1. If manifest index exists and is valid:
   - load explicit active edges;
   - load observed only when requested by mode;
   - load active workspace snapshot paths;
   - load dirty overlay paths;
   - load referenced repo materialization paths.
2. If manifest index is missing/corrupt:
   - fall back to current registered-project sidecar filtering;
   - emit a storage-health warning;
   - schedule/allow manifest repair.

The fallback reason must distinguish:

- `missing_not_migrated`: expected before Phase 3 rollout;
- `corrupt`: invalid JSON/schema/path reference;
- `stale`: index references an absent workspace manifest or snapshot.

Only `corrupt` and `stale` should page the operator in health output.

This avoids turning snapshot directories into a worse recursive glob.

### 3.4 Storage Health Integration

Extend `bbox_storage_health` to report:

- active snapshots;
- inactive snapshots;
- dirty overlays;
- manifest-index validity;
- cached snapshot bytes;
- prunable snapshot bytes.

### 3.5 Observed Retention Gate

Before Phase 3 completes, choose and document one:

- retain observed history indefinitely with health warnings;
- cap by bytes per provider/project;
- archive/compress old observed lanes;
- prune observed lanes older than a configured window.

The architecture doc currently carries `phase_3_policy_gate: true`; this phase
must close it.

### 3.6 Tests

Tests:

- active loader reads only manifest-index active paths;
- inactive snapshot file does not affect `bbox_describe_schema`;
- corrupt manifest index falls back safely;
- manifest repair recreates active index from manifests;
- concurrent manifest writers serialize without losing either update;
- sidecar watcher/signature code ignores inactive snapshot directories unless
  the active manifest changes;
- storage health reports inactive/prunable snapshot bytes.

**Acceptance gate:** active EdgeIndex rebuild is O(number of registered
workspaces + active lanes), not O(total cached snapshots).

**Rollback:** delete/ignore manifest index and use current registered sidecar
filter.

## Phase 4: Snapshot Writes and Dirty Overlay

**Prerequisites:** Phase 3.

**Goal:** model branch/worktree state directly.

### 4.1 Snapshot ID

Implement helpers:

```rust
clean_snapshot_id(repo_id, project_id, head_sha, indexer_version, chunker_version)
nongit_snapshot_id(project_id, source_tree_fingerprint, indexer_version, chunker_version)
```

Do not include dirty fingerprint in clean snapshot ids.

### 4.2 Snapshot Writer

Write workspace materialization under:

```text
edges/materialized/workspace/<project_id>/snapshots/<snapshot_id>/
  project.jsonl
  symbols.jsonl
  git-current.jsonl
```

Use temp directory + atomic manifest switch:

1. write snapshot files into `<snapshot_id>.tmp-<pid>`;
2. fsync files;
3. rename temp dir to `<snapshot_id>`;
4. acquire the manifest-index writer lock from Phase 3;
5. reread current workspace manifest/index;
6. update workspace manifest;
7. update manifest index;
8. release the lock.

If rename-across-filesystem is a concern, keep temp dirs under the same parent.

### 4.3 Dirty Overlay Writer

Write dirty overlay under:

```text
edges/materialized/workspace/<project_id>/dirty-current/
  project.jsonl
  symbols.jsonl
  git-current.jsonl
  overlay_manifest.json
```

V1 merge granularity is per relative path:

- overlay file lists the relative path hashes it covers;
- active loader suppresses clean snapshot workspace facts whose source or
  target `project_file` carries those path hashes;
- active loader loads overlay facts for those paths;
- unchanged paths come from the clean snapshot.

Symbol facts that cannot be tied back to a path hash must not participate in
per-file overlay merging. Phase 4 starts with a whole-workspace dirty overlay
unless the implementation proves path-hash suppression is complete for project
files, symbols, and git-current edges. The whole-workspace overlay is less
cache-efficient but keeps the correctness contract simple and bounded.

Dirty overlay files only accept `Derived` materialized edges. The writer rejects
explicit or observed edges before writing, and the loader reports a health error
if an existing overlay contains non-derived provenance.

### 4.4 Branch Switch Flow

On reindex for a registered git project:

1. read `HEAD`, branch, git common dir, worktree dir, dirty fingerprint;
2. ensure clean snapshot for `HEAD` exists;
3. if dirty, write/replace dirty overlay;
4. if clean, remove stale dirty overlay only after validating it contains
   exclusively derived materialized edges; otherwise quarantine it and report a
   health warning;
5. update manifest;
6. schedule GC for old snapshots after the configured grace.

### 4.5 Worktree Flow

On project registration:

- canonical path still yields `project_id`;
- git root/common-dir yields `repo_id`;
- worktree path is stored in manifest;
- external worktrees become separate `project_id`s sharing `repo_id`;
- nested `.worktrees` remain skipped by scanner unless registered directly.

### 4.6 Tests

Tests:

- branch A and branch B produce different clean snapshot ids;
- switching back to branch A reuses its cached snapshot;
- dirty file rewrite updates `dirty-current`, not a new snapshot;
- clean checkout removes dirty overlay;
- clean checkout quarantines dirty overlay that contains explicit/observed
  provenance;
- external worktree shares `repo_id` but has different `project_id`;
- inactive snapshots do not affect active schema counts;
- whole-workspace dirty overlay wins over clean snapshot;
- per-file overlay wins over clean snapshot for covered paths if enabled.

**Acceptance gate:** branch switches and dirty edits no longer create stale
active graph facts or unbounded sidecar growth.

**Rollback:** manifest can point back at managed derived sidecars from Phase 2.

## Phase 5: Legacy Extraction and Lane Migration

**Prerequisites:** Phase 4 active snapshot path is stable.

**Goal:** move old legacy sidecar content into lifecycle-owned lanes.

### 5.1 Apply Extraction

Implement:

```text
bbox_storage_migrate_legacy_edges(project?: string, dry_run: bool = true)
```

Apply path:

1. acquire a per-project migration lock;
2. confirm managed materialized replacement exists for the project;
3. read legacy sidecar and compute source hash without holding the global
   manifest-index writer lock;
4. if a migration manifest for the same source hash is already committed, exit
   successfully;
5. write explicit, observed, and quarantine outputs to
   `edges/migrations/<migration_id>/staging/`;
6. write a pending migration manifest with source hash and counts;
7. acquire the manifest-index writer lock for the short critical section;
8. atomically install the lane outputs with temp-file + rename;
9. rename old sidecar to bounded backup location;
10. mark the migration manifest committed;
11. update manifest index/storage health;
12. release the manifest-index writer lock;
13. release the per-project migration lock.

Crash recovery is part of the apply path. On startup or the next migration run:

- `pending` with source sidecar still present: remove staging outputs and retry;
- `pending` with source sidecar already moved: verify installed lane hashes,
  then mark committed;
- committed manifest with source sidecar present: ignore lane outputs and report
  duplicate-source health error until the operator resolves it;
- committed manifest with backup missing: keep lane outputs active, but pin a
  health warning because rollback is no longer possible.

Active loading must not load both the old legacy sidecar and newly installed
lane outputs for the same committed migration.

### 5.2 Quarantine

Quarantine path:

```text
edges/quarantine/<project_id>/<timestamp>.jsonl
```

Each line:

```json
{
  "source_path": "...",
  "line_number": 123,
  "raw": "...",
  "error": "serde error"
}
```

Quarantine is never loaded into active graph state. It is reported by storage
health and prunable only by explicit operator action.

### 5.3 Tests

Tests:

- extraction preserves explicit edges;
- extraction preserves observed/tool edges in observed lane;
- extraction drops derived edges only after replacement exists;
- malformed lines are quarantined;
- repeated migration is idempotent;
- crash after staging but before sidecar rename is idempotent;
- crash after sidecar rename but before committed marker is idempotent;
- active loader does not double-load old sidecar plus migrated lanes;
- backup retention applies to migrated legacy files.

**Acceptance gate:** legacy `edges/<project_id>.jsonl` no longer carries normal
derived or observed traffic, and existing durable facts still load.

**Rollback:** migration manifest records source backup path and hashes so a
project can restore the old sidecar if needed. Migration backups are pinned from
normal backup GC until the migration has survived at least one release cycle or
the operator explicitly unpins them.

## Phase 6: V2 Entity Refs

**Prerequisites:** Phase 5.

**Goal:** make snapshot-specific refs explicit without breaking old refs.

### 6.1 Parser

Add new entity types:

```text
project_file_v2:<project_id>:<snapshot_id>:<rel_path_hash>:<chunk_hash>:<idx>
symbol_v2:<project_id>:<snapshot_id>:<qualified_name>:<defn_hash>
```

Do not overload old refs by segment count.

### 6.2 Resolver

Resolution order:

- old refs search active snapshot first, then explicit requested historical
  mode;
- v2 refs resolve exactly to their snapshot;
- historical mode can resolve inactive snapshots if retained;
- missing retained snapshot returns a clear stale-ref error with suggested
  search fallback.

### 6.3 Producer Cutover

Only new project indexing emits v2 refs after a feature flag:

```text
BBOX_PROJECT_REFS_V2=1
```

Do not rewrite old transcripts, notes, or knowledge entries.

Snapshot ids improve exact historical resolution, but they also make refs less
portable across machines and across retained-snapshot GC boundaries. The parser
and providers are always installed; producer emission remains gated by
`BBOX_PROJECT_REFS_V2=1`.

### 6.4 Tests

Tests:

- old refs parse and resolve;
- v2 refs parse and resolve exact snapshot;
- old ref fallback does not accidentally search backups;
- stale v2 ref returns actionable error;
- `bbox_describe_schema` can count v1 and v2 refs coherently.

**Acceptance gate:** v2 refs add precision without breaking old corpus data.

**Rollback:** disable v2 emission flag; parser can remain.

## Cross-Phase Acceptance Suite

The shipped focused and cross-phase tests cover this scenario:

1. register repo on branch A;
2. index project;
3. switch to branch B with different symbols;
4. index project;
5. assert active graph has branch B symbols only;
6. switch back to branch A;
7. assert cached branch A snapshot reactivates;
8. make dirty edit;
9. assert dirty overlay wins and no extra clean snapshot appears;
10. run storage GC dry-run;
11. assert active snapshot and dirty overlay are retained.

The fixture can be a temp git repo created inside a unit test. Keep it small:
two branches, one file, one symbol rename.

## Deployment Notes

These were the intended shipping cuts; no item in this list remains as a
pending task in this record.

1. Phase 1 only: health + GC visibility.
2. Phase 2 only: no new derived append growth.
3. Phase 3 with fallback loader enabled.
4. Phase 4 behind a feature flag:
   `BBOX_STORAGE_SNAPSHOTS=1`.
5. Phase 5 migration dry-run only.
6. Phase 5 apply after at least one release cycle of dry-run reports.
7. enable Phase 6 emission with `BBOX_PROJECT_REFS_V2=1` only for callers that
   require exact snapshot refs.

## Operational Metrics

Expose counters in logs and health output:

- active edge files;
- inactive snapshot files;
- backup bytes;
- observed bytes;
- orphan bytes;
- EdgeIndex rebuild milliseconds;
- active refs loaded;
- skipped inactive refs;
- GC candidates and applied bytes;
- dirty overlay rewrites.

The success condition is not only lower disk use. It is that users can switch
branches, use worktrees, and run the daemon for weeks without learning where
edge sidecars live.

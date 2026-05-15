---
title: "Blackbox roadmap \u2014 prospective work tracker"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - corpus
---

# Blackbox roadmap — prospective work tracker

## Problem

The existing bbox surfaces each cover a narrow temporal band:

| Tool | Temporal band | What it answers |
|---|---|---|
| `bbox_inbox` | **past** (reactive) | What broke, what stalled, what contradicts |
| `bbox_thread` | **present** (active) | What is being worked on right now |
| `bbox_knowledge` | **durable** (atemporal) | What has been decided, what rules bind |

Missing is the **future** band — designed-but-not-implemented, planned features,
deferred ideas, exploration candidates, acknowledged technical debt. A user
sitting down to a codebase after a week away has no native query for "what
should I work on next." Inbox items are too noisy for this (every surprise
generates an entry); threads already imply someone is doing them; knowledge
entries are atemporal conventions.

A roadmap tracker fills the gap: **persistent, scannable, linkable to both
design docs and originating/deferred threads, with a one-way render to a
version-controlled `ROADMAP.md` that mkdocs (or any static site generator)
can consume directly.**

## Non-Goals

- **Not a replacement for threads.** A roadmap item that is currently being
  worked on *is* a thread. The roadmap owns the *intent*; the thread owns the
  *execution*. Active/completed state is derived from linked thread lifecycle.
- **Not a project management tool.** No estimates, no assignments, no
  capacity planning, no Gantt charts. This is a lightweight "designed but not
  done" list with edge types that let agents and humans traverse the landscape.
- **Not a two-way render.** `ROADMAP.md` is an output artifact, like
  `CLAUDE.md`. It is never absorbed back. Manual edits to it are overwritten on
  the next render.
- **Not an inbox replacement.** Surprises, blockages, and contradictions
  remain in inbox. Roadmap items are *deliberately authored* future work, not
  automatically surfaced loose ends.

## Entity type: `roadmap_item`

Stored alongside knowledge entries under `~/.blackbox/roadmap.json` (global)
and `<project>/.bbox/roadmap.json` (project-scoped). Indexed into the agentic
corpus as entity type `roadmap_item`. The canonical entity ref is
`roadmap_item:<id>` (e.g. `roadmap_item:a1b2c3d4`), matching the existing
`EntityRef` enum pattern. A new `EntityRef::RoadmapItem { id }` variant is
added to `src/entity_ref.rs`.

### Fields

| Field | Type | Description |
|---|---|---|
| `id` | `roadmap-<8hex>` | Stable identifier (stored; canonical ref is `roadmap_item:<id>`) |
| `title` | string | Short name (1–6 words) |
| `body` | string | Design notes, motivation, constraints, acceptance criteria |
| `status` | enum | **Persisted:** `proposed`, `accepted`, `deferred`, `rejected` |
| `category` | enum | `feature`, `refactor`, `exploration`, `debt`, `risk`, `infrastructure` |
| `priority` | enum | `high`, `medium`, `low` |
| `scope` | enum | `global`, `project` |
| `project` | string? | Project path (for project-scoped items) |
| `created_at` | ISO 8601 | When authored |
| `updated_at` | ISO 8601 | Last mutation |
| `transitions` | array | `[{status, at, note?, actor?, source?}]` — full audit trail |

### Lifecycle

**Persisted statuses** (what the roadmap item itself tracks):

```
proposed → accepted → deferred → (back to accepted)
  ↓          ↓
rejected   (promoted to thread — see below)
```

- **proposed**: suggested, not yet reviewed
- **accepted**: acknowledged as worth doing, not started
- **deferred**: accepted but intentionally paused (linked back to the thread or
  decision that deferred it via `ROADMAP_DEFERRED_FROM`)
- **rejected**: reviewed and declined (reason in body)

**Computed statuses** (derived from linked thread state, never stored):

- **in_progress**: at least one unresolved `ROADMAP_SPAWNS` thread exists
  (thread status is not `resolved`)
- **done**: all `ROADMAP_SPAWNS` threads are resolved

This separation avoids duplicating thread state in roadmap items. The roadmap
owns the *intent* (what we plan to do); the thread owns the *execution* (are
we doing it, is it done). When the last spawned thread resolves, the render
and query surfaces mark the item as done — but no status field changes in the
roadmap item itself.

Status transitions are validated at the store level: `deferred` items can
return to `accepted`; `rejected` items can be reopened to `proposed`.
Transition history is recorded in the `transitions` array, giving a full
audit trail without overloading a single timestamp field.

Each transition entry:
```json
{ "status": "accepted", "at": "2026-05-08T12:00:00Z", "note": "Reviewed by codex bro", "actor": "codex-gpt55", "source": "thread-a1b2c3d4" }
```

The `source` field points to the entity ref that caused the transition
(e.g. the thread that deferred it, the session that proposed it).

## Edge types

Roadmap items form typed edges to other corpus entities. These edges are
stored via the generic `bbox_link` graph surface (see `design/corpus/commit-work-provenance.md`)
and surfaced by `bbox_inspect_entity`. Roadmap-specific link actions on the
`bbox_roadmap` tool are domain-validated convenience wrappers over `bbox_link`;
they do not create a second edge persistence path.

All edge endpoints use canonical entity refs (`roadmap_item:<id>`, `thread:<id>`,
`project_file:<project_id>:<hash>:<hash>:<n>`), never bare ids.

### `ROADMAP_SPAWNS`

Directed from `roadmap_item` → `thread`. When an accepted roadmap item is
promoted to active work (via `bbox_roadmap action="promote"`), the thread that
carries the work is linked here. Multiple threads can spawn from one roadmap
item (e.g. a feature broken into phases).

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "thread:f8e9d0a1", "kind": "ROADMAP_SPAWNS", "at": "<ISO>" }
```

When the last spawned thread resolves, the item is rendered/computed as done.
If all spawned threads are cancelled and the item returns to `accepted`, the
edges remain (as historical record) but the item's computed status drops back.

### `ROADMAP_DEFERRED_FROM`

Directed from `roadmap_item` → `thread` (or `knowledge` entry with a decision).
Captures the provenance of a deferral: *this roadmap item was deferred by
thread X because Y*. Answers "why is this not being worked on" and lets the
user re-evaluate if the deferral condition has changed.

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "thread:f8e9d0a1", "kind": "ROADMAP_DEFERRED_FROM", "note": "<summary>", "at": "<ISO>" }
```

### `ROADMAP_DESIGNED_IN`

Directed from `roadmap_item` → `project_file` entity ref. Links the roadmap
item to the indexed design doc(s) that describe it. This is the bridge
between "what we plan to build" and "what we've already written down in
design docs."

```json
{
  "from": "roadmap_item:a1b2c3d4",
  "to": "project_file:8f3a1b2c:ab12cd34:ef56gh78:0",
  "kind": "ROADMAP_DESIGNED_IN",
  "at": "<ISO>",
  "file_path": "design/corpus/roadmap.md",
  "section_anchor": "#blackbox-roadmap-prospective-work-tracker"
}
```

The edge carries `file_path` (the relative path at link time) and optional
`section_anchor` alongside the content-hash `project_file` ref. On render and
inspect, both are used for link health:

- **Healthy**: chunk hash resolves to current file at `file_path` → render as
  a link with section anchor.
- **Stale**: chunk hash doesn't match but `file_path` still exists → render as
  a link without anchor, append `[stale]` marker. The content has changed;
  the section may have moved.
- **Missing**: `file_path` doesn't exist → render as plain text with
  `[missing: design/corpus/roadmap.md]`. The file was renamed or deleted.

Agents calling `bbox_hybrid_search` for design docs can see which roadmap
items reference them; agents calling `bbox_inspect_entity` on a roadmap item
can traverse to the design docs.

#### Link repair

`bbox_roadmap(action="repair_links")` re-resolves all `DESIGNED_IN` edges
against the current agentic corpus index. For each edge, it searches for the
current `project_file` chunk that best matches the original `file_path` and
content. If a high-confidence match is found, the edge metadata (`file_path`,
chunk ref, `section_anchor`) is updated. If no match is found, the edge is
left intact and reported as unresolved. This prevents silent relinking to the
wrong section.

### `ROADMAP_DEPENDS_ON`

Directed from `roadmap_item` → `roadmap_item`. Item A depends on item B being
done first.

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "roadmap_item:e5f6g7h8", "kind": "ROADMAP_DEPENDS_ON", "at": "<ISO>" }
```

### `ROADMAP_BLOCKED_BY`

Directed from `roadmap_item` → `roadmap_item` (or `thread`, or `knowledge`
entry). Item A is blocked by B. Stronger than `DEPENDS_ON`: implies the
blocker is not under roadmap control (e.g. blocked on an external dependency,
a decision, or a thread that isn't a spawned work item).

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "roadmap_item:e5f6g7h8", "kind": "ROADMAP_BLOCKED_BY", "note": "<reason>", "at": "<ISO>" }
```

### `ROADMAP_SUPERSEDES`

Directed from `roadmap_item` → `roadmap_item`. The newer item replaces the
older one.

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "roadmap_item:00000000", "kind": "ROADMAP_SUPERSEDES", "at": "<ISO>" }
```

### `ROADMAP_SUBSUMES`

Directed from `roadmap_item` → `roadmap_item`. A larger item absorbs a
smaller one (e.g. a broad refactor subsumes several individual debt items).

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "roadmap_item:e5f6g7h8", "kind": "ROADMAP_SUBSUMES", "at": "<ISO>" }
```

### `ROADMAP_RELATED_TO`

Directionless (stored bidirectionally). Truly symmetric association between
roadmap items — no implication of ordering, dependency, or ownership.

```json
{ "from": "roadmap_item:a1b2c3d4", "to": "roadmap_item:e5f6g7h8", "kind": "ROADMAP_RELATED_TO", "at": "<ISO>" }
```

## MCP tool: `bbox_roadmap`

```
bbox_roadmap {
  action: "create" | "get" | "list" | "update" | "delete"
        | "search" | "next" | "promote"
        | "link" | "unlink" | "repair_links"

  // create/update fields
  title?, body?, status?, category?, priority?,
  scope?, project?,

  // lookup
  id?,              // get/update/delete by id
  query?,           // free-text search (title + body)
  status?,          // filter list/search (accepted computed statuses too)
  category?,        // filter list/search
  project?,         // filter list/search
  limit?,

  // linking (domain-validated wrappers over bbox_link)
  link_type?,       // spawns | deferred_from | designed_in
                    // | depends_on | blocked_by | supersedes | subsumes | related_to
  link_target?,     // entity ref (roadmap_item:<id>, thread:<id>, project_file:...)
  link_note?,       // annotation for the edge

  // promote
  brofile?,         // brofile to dispatch (defaults to project default)
  project_dir?,     // working directory for the spawned thread

  // repair
  dry_run?,         // preview without updating edges (default: false)
}
```

### `action="next"`

Ranks accepted/browsable roadmap items by a composite score: priority weight,
blocker count (penalized), staleness (older accepted items scored higher), and
design-link health (stale/missing links penalized). Returns top-N items
(default N=5).

Excludes items with active `ROADMAP_BLOCKED_BY` edges by default. Pass
`include_blocked=true` to include them (e.g. for triage review).

Computed `in_progress` items (accepted + unresolved spawned threads) are
included as a separate section; computed `done` items are excluded.

### `action="promote"`

Takes a roadmap item id. Opens a new `bbox_thread` with the item's title as
the thread topic and the item's body + linked design doc excerpts injected as
the thread's opening note. Records a `ROADMAP_SPAWNS` edge. Returns the
thread id.

Idempotent: if the item already has an unresolved `ROADMAP_SPAWNS` thread,
returns the existing thread id instead of opening a new one. This prevents
accidental multi-thread spawns from repeated promote calls.

### `action="link"` / `action="unlink"`

Domain-validated convenience wrappers over `bbox_link`. Validations:
- `spawns` link target must be a `thread` entity ref
- `deferred_from` link target must be a `thread` or `knowledge` entity ref
- `designed_in` link target must be a `project_file` entity ref; also sets
  `file_path` and `section_anchor` on the edge from the referenced chunk
- `depends_on`, `blocked_by`, `supersedes`, `subsumes`, `related_to` link
  targets must be `roadmap_item` entity refs

Underneath, all edges go through the `bbox_link` / `WorkGraphLink` surface
defined in `design/corpus/commit-work-provenance.md`. Roadmap edges are not a
separate persistence path.

### `action="repair_links"`

Re-resolves all `ROADMAP_DESIGNED_IN` edges for the item (or all items if no
`id` specified). For each edge, queries the current agentic corpus for the
best-matching `project_file` chunk using the original `file_path` as a search
hint. If a high-confidence match is found, updates the edge's chunk ref,
`file_path`, and `section_anchor`. If no match, leaves the edge intact and
reports it as unresolved.

### Client-facing rules

- **List Before Create** (per AGENTS.md convention): call `action=list` with
  the candidate title query before `action=create` to avoid duplicates.
- Deferred items should record a `ROADMAP_DEFERRED_FROM` edge linking to the
  thread or decision that deferred them.
- Items linked via `designed_in` accept `project_file` entity refs from
  `bbox_refactor_project_refs` or `bbox_hybrid_search`.

## Render: one-way to `ROADMAP.md`

The render pipeline (`src/render.rs`) already emits `CLAUDE.md`, `AGENTS.md`,
and `GEMINI.md`. Adding `ROADMAP.md` follows the same pattern:

1. **Trigger**: `bbox_render(scope="project", format="roadmap")` or included
   automatically in `scope="both"` renders.
2. **Output**: `<project>/ROADMAP.md`, written fresh each render (not
   surgically patched like global memory files — `ROADMAP.md` is fully
   generated).
3. **Format**: Markdown with mkdocs-compatible structure. Grouped by status
   (computed) then category, with priority badges, links to design docs, and
   thread references.

### Render template (example output)

```markdown
<!-- Generated by blackbox — do not edit. Regenerate with bbox_render. -->

# Roadmap — <project name>

## In Progress

### Feature: Add roadmap tracker
- **Priority:** high
- **Designed in:** [`roadmap.md#entity-type-roadmap_item`](roadmap.md)
- **Thread:** `thread-a1b2c3d4`
- Design notes, motivation, and acceptance criteria...

## Accepted

### Refactor: Consolidate EdgeIndex compaction
- **Priority:** medium
- **Designed in:** `src/index/compaction.rs` [stale]
- **Depends on:** ~~`roadmap_item:completed-prereq`~~ ✓
- Merge dual-compaction paths...

### Debt: Remove ARC_PRODUCED_COMMIT / COMMIT_PRODUCED_BY_ARC ghosts
- **Priority:** low
- **Designed in:** [missing: src/orchestration/commit.rs]
- These edge types are defined in provider schemas but never emitted...

## Proposed

### Exploration: Tantivy columnar storage for numeric fields
- **Priority:** medium
- Tantivy 0.22+ supports columnar storage...

## Deferred

### Feature: Multi-tenant bbox daemon
- **Priority:** low
- **Deferred from:** `thread-f8e9d0a1` — blocked on Forgejo federation MVP
- ...

## Done

### Feature: Rule-packet compilation
- **Priority:** high
- **Completed:** 2026-05-01 (thread `thread-b2c3d4e5`)
- ...
```

### Stale-link rendering

`ROADMAP_DESIGNED_IN` edges carry both a content-hash `project_file` ref and
a human-readable `file_path`. The renderer resolves both:

| Chunk match | File exists | Rendered as |
|---|---|---|
| Yes | Yes | Linked with path + anchor |
| No | Yes | Linked without anchor, `[stale]` suffix |
| — | No | Plain text, `[missing: path]` suffix |

Stale and missing links are call-out signs for the reader to investigate,
not errors that block the render.

### mkdocs integration

Mkdocs renders `docs/` as its source tree by default. To expose the roadmap
as a mkdocs page:

1. Render `ROADMAP.md` inside the mkdocs source dir:
   ```bash
   bbox_render scope=project format=roadmap project=/path/to/repo
   ```
   writes `<project>/docs/roadmap.md` (pathing controlled by a configurable
   `roadmap_output_path` field in `project` store, defaulting to
   `<project>/ROADMAP.md`).

2. Mkdocs picks it up automatically if `docs/` is the configured docs_dir:

   ```yaml
   # mkdocs.yml
   nav:
     - Home: index.md
     - Roadmap: roadmap.md
     - Design Docs: design/
   ```

3. Because `ROADMAP_DESIGNED_IN` edges carry file paths, the renderer emits
   relative links to the design docs — making the roadmap a navigable index
   into the design directory.

### Global roadmap

A global roadmap (items with `scope="global"`) renders to
`~/.blackbox/ROADMAP.md`. This is the cross-project list of infrastructure
concerns, non-project-bound explorations, and global tooling improvements.

## Discovery: `bbox_discover_seed_entities`

Roadmap items are indexed into the agentic corpus as entity type
`roadmap_item` with canonical ref `roadmap_item:<id>`. Free-text queries like
"what's planned for compaction" will surface matching items. Combined with the
edge types, an agent can:

1. Search for a concept → find a roadmap item
2. Inspect the item → see `designed_in` links to design docs
3. Traverse `spawns` to find the active thread
4. Check `deferred_from` to understand why something paused
5. Follow `depends_on` / `blocked_by` to find prerequisites

No new discovery primitives needed — the existing graph traversal tools cover
this. The entity ref uses the canonical `roadmap_item:<id>` form so
`bbox_inspect_entity` and `bbox_find_paths` work without special-casing.

## Integration with `EntityRef`

A new variant is added to `src/entity_ref.rs`:

```rust
pub enum EntityRef {
    // existing variants ...
    RoadmapItem { id: String },
}
```

Parsing: `roadmap_item:<8hex>` → `EntityRef::RoadmapItem { id }`.
Display: `EntityRef::RoadmapItem { id }` → `roadmap_item:<id>`.

The `EntityType` enum gains `RoadmapItem` for filtering/schema purposes.

## Storage

Two layers, matching the knowledge store pattern:

| Scope | Path | Indexed |
|---|---|---|
| Global | `~/.blackbox/roadmap.json` | Yes (entity type `roadmap_item`) |
| Project | `<project>/.bbox/roadmap.json` | Yes |

Format: JSON array of items, loaded on demand (not held in memory).
Mutations write atomically via write-to-temp + rename, same as the knowledge
store.

Edges are stored in the agentic corpus edge sidecar (`edges/<project_id>.jsonl`
for project items, `edges/global.jsonl` for global items) via the existing
`bbox_link` / `WorkGraphLink` path. Roadmap link actions are wrappers, not a
separate edge persistence mechanism. This means a `bbox_edge_compact` pass
covers roadmap edges too.

## Integration points

| Concern | Existing path | Roadmap hook |
|---|---|---|
| Entity ref | `src/entity_ref.rs` | Add `EntityRef::RoadmapItem { id }` variant |
| Search | `bbox_hybrid_search` | `doc_type=roadmap_item` filter |
| Inspect | `bbox_inspect_entity` | Returns roadmap edges (via WorkGraphLink) |
| Find paths | `bbox_find_paths` | Traverses `ROADMAP_*` edge kinds |
| Render | `render.rs` render pipeline | `render_roadmap()` function |
| Agentic index | Tantivy schema | Add `roadmap_item` to doc_type enum |
| Tool docs | `tool_docs.rs` | `bbox_roadmap` stanza |
| MCP dispatch | `main.rs` tool registry | `#[tool]` handler |
| Edge storage | `bbox_link` / WorkGraphLink | Reused — no new storage path |

## Migration path

No migration — this is a new store. Projects without a roadmap stay exactly as
they are today. The first `bbox_roadmap(action="create")` call creates the
store file.

## Future considerations

- **packet-driven nudge**: a rule-packet could watch for "roadmap item X has
  been in `accepted` state for N days with priority `high`" and emit an inbox
  note.
- **brofile lens injection**: when a roadmap item is promoted to a thread via
  `ROADMAP_SPAWNS`, the item's body + design doc links could be injected as
  ambient context on the first dispatch.
- **Council / whiteboard integration**: roadmap items could be registered as
  topics on whiteboards during design-phase deliberation, with the resolution
  recorded back on the roadmap item's `transitions` array.
- **`bbox_inbox` integration**: `action="next"` could drive an inbox section
  that surfaces the top-N actionable roadmap items alongside surprises and
  followups.

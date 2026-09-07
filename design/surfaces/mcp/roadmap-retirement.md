---
title: "Roadmap mutation retirement and historical access"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - surfaces
  - mcp
brief: "Freeze roadmap mutations while preserving historical data, graph relations, and exact read/export paths."
---

# Roadmap mutation retirement

The MCP milestone retires creation, editing, deletion, promotion, link changes,
and link repair. The existing tool remains a historical reader. This is a
write-surface retirement, not a store deletion or a claim that historical work
has completed.

## Evidence and disposition

The source baseline for this review is `b6189fbf`. Read-only live inspection of
`gap-56c74f23` and the roadmap list/get surfaces found one retained proposed
item, one status transition, and no links or spawned threads. The item belongs
to another private project. Its content and ownership path are deliberately
not reproduced here. That record must not be copied into this public repo's
graph or silently marked completed.

The gap's claim that a campaign/inquiry substrate is missing is stale:
`.bbox/graphs/design/schema.json` already defines `dsg:Campaign`, `dsg:Inquiry`,
and `dsg:Concept`. The graph contains campaign and inquiry instances.
`scripts/design-graph` provides guarded authoring, transactional `apply`,
`check`/`lint`, and the `frontier`/`blockers` planning reads. See
[design graph operations](../../../docs/design-graph.md) for its authority
boundary. It is a repository-owned workflow, not a generic daemon replacement
for arbitrary projects' planning data.

| Consumer | Preserved outcome |
|---|---|
| `src/tools/roadmap.rs` | Bounded get/list/search, historical next ranking, inline render/template and exact body export |
| `crates/bbox-providers/src/providers/roadmap_item.rs` | Stable roadmap entity refs and stored properties, with historical lifecycle metadata |
| `crates/bbox-indexing/src/index/roadmap_docs.rs` | Existing index eligibility and embedding identities; no index purge |
| `crates/bbox-edge-index/src/edge_index.rs` | All eight stored roadmap relationship kinds remain projected and traversable |
| `src/server/state.rs` and catalog owner adapters | Durable store and project ownership preservation; no startup rewrite or deletion |
| `docs/roadmap.md` and `roadmap.tera` | Existing historical rendered artifact and caller-owned template |

Source search found no separate user-facing roadmap mutation CLI. The internal
store/domain methods remain for compatibility and ownership maintenance; the
MCP action gate runs before deserialization into those mutation methods.
Existing retained records are real consumers, even if the active item count is
small. Name similarity or low population is not a deletion criterion.

## Caller contract

Discovery advertises only `get`, `list`, `search`, `next`, `render`, and
`default_template`. Old mutation strings still deserialize so they receive
`error.roadmap_mutation_retired`, with `isError=true`, before any roadmap,
thread, index, or persistence effect. There is no opt-out flag.

Every successful MCP projection says `lifecycle=historical_read_only`.
`detail=body` recovers the original read value, not a rewritten record. Existing
cursor freshness checks and bounded summaries remain. `next` is explicitly a
read-only ranking of retained accepted records; it does not nominate new work
or open a thread. The historical default render excludes delivered/rejected
items, so an unfiltered list/body read is the complete inventory/export path.

The schema advertises the retained `Roadmap` edge family and marks both it and
`roadmap_item` as historical. Removing those edges would erase provenance, and
repointing them without a per-item mapping would assert false identity. Neither
happens here.

For new concepts and inquiries in this repository, use the existing design
writer. List/show before creating and preserve its proposed-versus-ratified
commitment gate. For active execution, read the historical source and explicitly
open a thread with necessary context. This does not create a migration link or
change the historical item. Other projects choose their own planning owner.

## Remaining owner decision

The remaining dependency is a per-owner disposition for surviving records,
not missing campaign/inquiry schema. For each historical record, the owner must
choose retention as history or an explicit conversion in the owning project's
planning substrate. A conversion needs stable source identity, a status/history
mapping, scope/privacy preservation, link mapping, and duplicate detection.
The inspected private record has no links to map, but its destination graph and
operator-approved disposition are not established by this public repo's state.
No concrete destination can be safely inferred.

Keep the current historical readers/store/edges until that decision is complete.
A future removal needs proof that every retained owner can still recover its
original record and follow mapped relations. Do not close the entire gap merely
because mutation retirement has landed.

Proposed update for `gap-56c74f23` (not sent to production by this milestone):

> Campaign, inquiry, and concept schema plus guarded authoring and planning
> reads already exist in the repository design graph. The MCP roadmap mutation
> surface is now retired; historical reads, exact export, indexing, ownership
> adapters, and graph relations remain. The remaining work is per-owner
> disposition and any explicitly approved historical-item mapping. The sole
> inspected record belongs to another private project and has no links; it is
> retained without copying content or changing status. Store/edge deletion is
> not authorized by the retirement milestone.

## Validation

Written regressions exercise every retired mutation against a populated
throwaway store, compare complete historical data before/after, flush and reopen
it, retain relations and thread counts, and recover oversized Unicode bodies
through the real read adapter. Schema tests cover closed read-action discovery
and retained historical graph vocabulary/population. Tool-doc tests verify
replacement and retirement guidance.

The edit-only worktree runs the pinned formatter and diff checks. The
orchestrator runs workspace nextest with roadmap/schema/tool-doc filters, the
full workspace gate, and concurrency lint in its warm lane. Served HTTP checks
should confirm all mutation refusals have no side effects, body-page recovery,
and historical lifecycle metadata without writing production records.

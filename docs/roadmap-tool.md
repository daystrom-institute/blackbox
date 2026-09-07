# Historical roadmap records

`bbox_roadmap` is a read-only compatibility surface. New roadmap writes,
promotion, link changes, and link repair are retired. Stored items, transition
history, and relationships remain intact. Retirement does not change an item's
stored status or declare its work completed.

## Read and export

- `get`, `list`, and `search` inspect retained items.
- `next` ranks retained accepted items using the historical ranking function.
  It is a read-only view, not an execution or commitment decision.
- `render` and `default_template` return inline projections for the caller to
  apply. The historical default template omits delivered and rejected items;
  `list` without status filters is the complete inventory.
- Summaries are bounded. `list`/`search` accept `limit` (default 20, max 100)
  and `offset`; `next` uses `n`/`offset`. Follow `next_offset` against the live
  view.
- `detail="body"` returns exact JSON body pages. Repeat the same read and
  selectors with `cursor=body.next_cursor`, concatenate `body.text`, and parse
  JSON. Rendered markdown and template source are JSON strings. Changed
  content or selection rejects a cursor. Omit row limits/offsets for body reads.

Every MCP read labels its lifecycle `historical_read_only`. The stored record
and the exact body retain their original fields and bytes. `roadmap_item:`
entity refs and the eight `ROADMAP_*` edge types remain available through graph
retrieval. Schema orientation identifies them as historical.

`create`, `update`, `delete`, `promote`, `link`, `unlink`, and `repair_links`
return `error.roadmap_mutation_retired` before changing roadmap or thread state.
Legacy requests get this specific refusal even though discovery advertises
only the six historical read actions. There is no bypass flag.

## New work

This repository already has `dsg:Campaign`, `dsg:Inquiry`, and `dsg:Concept`
vertices in its committed design graph. Use the existing
[design graph operations](design-graph.md): list/show before creating, stage
changes through `scripts/design-graph`, and use `apply --dry-run`, `check`,
and `lint` to validate. `frontier` and `blockers` are existing planning reads.
The documented commitment gate still applies: agents may propose, while
campaign status changes and other binding decisions require operator
ratification. The script is a checkout-owned writer, not a remote MCP mutation
endpoint, and does not mechanically enforce all human authority rules.

For active execution, read the historical item and explicitly open a
`bbox_thread` with the necessary context. This does not migrate the record or
create a new roadmap edge. Do not turn requested implementation into a new
planning entry merely to defer it.

Other projects retain ownership of their own plans. This repository's design
graph is not a migration destination for another project's private records.
Choose any conversion in the owning project, preserving source identity,
status history, scope, and links with an explicit mapping. No automatic
roadmap-to-graph converter is shipped by this milestone.

## Preservation

The configured roadmap JSON store and its project ownership adapters remain.
Catalog migration, project relocation, and persistence can still preserve and
re-scope historical data. Indexing and embedding continue to support retrieval.
No startup deletion, status rewrite, or graph-edge purge is introduced.

The checked-in [roadmap snapshot](roadmap.md) is historical output, not the
current planning source. The [retirement milestone](../design/surfaces/mcp/roadmap-retirement.md)
records consumer evidence and the remaining per-owner migration decision.

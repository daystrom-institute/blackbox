# Blackbox maintenance operations

Read this before running anything that changes daemon state, and when asked for routine maintenance.

Every operation here runs on the `ops` surface (`bro mcp call <tool> '<json>' --surface ops`) and needs operator authorization that covers it. Authorization for a named repair sequence covers the steps in that sequence; an operation outside it needs asking.

## Routine check

A routine pass is read-only unless it finds something:

```bash
bro mcp call bbox_doctor '{"format":"summary"}' --surface ops
bro mcp call bbox_embed_status '{}' --surface ops
bro mcp call bbox_storage_health '{}' --surface ops
```

Also list open threads with `bbox_thread_list(status="open")` so stalled investigations are visible. Report findings and proposed actions. Fixes are not part of the routine.

## Reindex

The daemon reindexes in the background on its own interval. A manual reindex is an intervention, not part of normal editing.

- `bbox_reindex(full=false)`: search is stale after recent changes, a project was just registered, or a background pass failed on a transient error.
- `bbox_reindex(full=true)`: the published source is current and the index still disagrees with it, counts look impossible, or the index was written by an incompatible version. On a large corpus this takes minutes and competes with every other caller.

Leave `wait` at its default; the call returns once the pass is admitted.

A reindex never recovers history no producer delivered, and it does not index project graphs. When the published source itself is stale, the fix is on the source host.

## Re-embed

`bbox_reembed(route="<route>")` rebuilds vectors for one route. Use the route names `bbox_embed_status` reports. Re-embed a route when its provider, model or dimension changed, when the provider was down and the route accumulated failures, or when vector search misses content keyword search finds after a reindex.

- Fix the provider or key first. Re-embedding against a broken provider only adds failures.
- Residue on non-transcript routes is swept automatically, so a coverage backlog alone is not a reason to re-embed repeatedly.
- The transcripts route is guarded because it reads the whole transcript corpus: it refuses unless `include_transcripts=true` is passed. Treat that as a separate, heavier operation to authorize.

Afterwards, watch `bbox_embed_status`: `queue_depth` should trend down and `last_error` should stay empty. A changed embedding key or provider config takes effect when the daemon restarts, which is the operator's action.

## Storage

1. `bbox_storage_health` reports totals and the largest contributors.
2. `bbox_storage_gc` previews by default and deletes nothing. Read the candidate and protection counts and show them to the operator.
3. Apply with `dry_run=false` once authorized. An apply runs a fresh plan, not the saved preview, and it is not atomic: deletions that completed before an error stay deleted. Status `partial` means the apply hit errors, not that nothing changed.

Receipts are short-lived daemon-local reports. Read the details you need promptly.

## Edge sidecar compaction

Only when legacy edge sidecars have grown. One project at a time:

```text
bbox_edge_compact(project_id="<id>", apply=false)
bbox_edge_compact(project_id="<id>", apply=true)
```

Review the dry run's removed and retained counts before applying. The tool writes a backup before it replaces the sidecar.

## What can be rebuilt

The search index, vector partitions and git metadata can be rebuilt from their sources through reindex and re-embed. Edge sidecars can be rebuilt too, but some need the owning host's code-source publication as well as a reindex. Knowledge, threads, gaps, the project registry and catalog, installed artifacts, task state, retained source deliveries and secrets cannot. Never propose deleting durable state as a fix, and do not assume a source the index was built from still exists elsewhere.

## Restarts

Restarting the daemon, a collector or the fleet supervisor is the operator's decision, because other agents depend on them. Restarting the fleet supervisor kills the agent sessions it is running. Name the service and the reason, then ask.

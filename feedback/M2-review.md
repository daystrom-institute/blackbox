# M2 + P2 fixes review

Commits `b349e34..ba193e5` (4 P2 fixes + M2 compaction arc).

## Issues (fix-forward)

1. **Verify the gate-eval-against-vars path actually works.** The
   `Decide` node has `gate: domain:embed/compaction-policy` and the
   packet's predicates reference `vars.vector_status.max_deleted_ratio`
   (dotted path through the vars dict). Per the workflow runbook,
   "Rules fire on the node's output (entity shape: `{output, node}`)"
   — meaning the gate sees the rendered prompt string, not the
   structured vars. If the engine merges vars into the gate entity,
   M2 works. If not, the gate always falls through to `skip_default`
   and compaction never fires.
   
   Add an integration test that sets up a partition with deleted_ratio
   > 0.3 and runs the workflow; assert it lands in the `compact`
   branch (not `skip`). If the test fails, the gate evaluation path
   needs extension; document the engine change required.

2. **`QuiesceSearch` and `SwapAtomic` are no-op markers.** Acceptable
   per code comment ("v1 is observable no-op") because vector rebuild
   reads WAL + writes derived files atomically anyway. But document
   in release notes: the workflow records intent; actual quiescence
   is implicit because reads serve from snapshot before the rebuild
   completes. If the in-process search ever becomes more concurrent,
   QuiesceSearch needs real implementation.

3. **`exec_read_vector_status::route_filter`** filters partitions
   by exact match. Makes sense. But the packet's `Decide` runs
   against ALL partitions — picks the worst (max deleted_ratio).
   Then `RebuildHnsw` rebuilds only that single worst partition. If
   multiple partitions are over the threshold, the others wait for
   the next cron tick. Acceptable; flag.

## Concerns

4. **Cron-routing packet** at `examples/agentic-corpus/packets/cron-routing/embed-compaction.json`
   triggers nightly. Verify the cron schedule is documented +
   reasonable (e.g. 3am local time when nothing else is busy). Nit
   if it's hardcoded to a less-ideal time.

5. **`SurfaceToInbox` for `notify` verdict** uses `bbox_note(kind=followup)`
   with project context. Good — operator can see "this partition
   needs compaction soon" via `bbox_inbox`. Verify the note's
   provenance (which arc emitted it) is preserved for traceability.

6. **`PartitionMetrics::deleted_ratio`** is a new field added in
   M2. Verify it's computed correctly: `deleted_count /
   (deleted_count + active_count)`. Edge case: all-deleted
   partition (active=0) should be 1.0, not divide-by-zero.

## P2 fix observations

7. **P2 fix #1 (commit anchor index)** — O(1) lookup via new
   `commit_anchor_index: HashMap<String, Vec<usize>>`. ✓

8. **P2 fix #2 (session tool calls index)** — O(1) lookup by
   `(provider, session_id)`. ✓

9. **P2 fix #3 (bound prior reads)** — time-since-edit bound
   limits prior reads to within ~20 turns. ✓

10. **P2 fix #4 (clarify blame modes)** — tool description
    expanded with two-mode behavior. ✓

## Nits

11. **`embed-compaction-nightly.json`** cron file — single-line
    JSON; verify the cron expression format.

12. **`exec_rebuild_hnsw`** validates route is non-empty. Good
    defensive check.

13. **The `Skip` node has `actor: ""` and no `on_enter` hooks** —
    just a terminal jump. Acceptable but could be omitted by
    routing the `skip` branch case directly to a terminal. Subjective.

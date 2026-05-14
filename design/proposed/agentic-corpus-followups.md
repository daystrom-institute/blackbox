# Agentic Corpus — Shipped-Phase Follow-ups

Status: implemented.
Related: `design/archive/agentic-corpus-release-notes.md` (original source of
the shipped-phase caveats).

## Resolution Summary

The follow-ups consolidated here are closed in the storage-performance branch.
They are no longer backlog items:

| Item | Resolution |
|---|---|
| F3 — schema migration workflow ops | `schema_migration_drop` and `schema_migration_rebuild` are real hook ops in `src/workflow/ops.rs`, and `schema-migration-arc.json` invokes them. |
| H3 — eval gate consumes shell output | `shell` hooks can capture stdout/exit data into workflow vars, `score_eval_output` turns harness output into a gate entity, and `nightly-eval-arc.json` routes on the captured verdict. |
| P1 — backfill tool-call edges on project register | `bbox_project_register` launches `backfill_tool_edges_for_project`, which walks prior transcripts and appends deduped observed tool edges for the newly registered project. |
| G2 — `notes.mergeStrategy = union` auto-config | `bbox_provenance_export` calls `ensure_notes_merge_strategy_union` before exporting git notes. |
| M2 — multi-partition vector compaction | `VectorStore::compact_partitions(None)` processes every eligible route; the periodic compactor uses it, and `embed-compaction-arc.json` invokes `compact_vector_partitions` rather than rebuilding only the worst route. |
| M3 — `task-completed` signal from bro finalize | bro task finalization emits `TaskCompleted` with `source_session`, `source_query`, and `task_kind`; the daemon routes it through `domain:auto-digest/task-completed-routing`. |
| M5 — auto-edge nightly trigger and scan cap | `auto-edge-nightly` cron routing is shipped, and `ExtractCandidatePairs` performs the real scan used by `auto-edge-arc`. |
| S4 — EdgeIndex memory model at scale | Edge storage is arena-backed with compact `EdgeKey` dedup keys instead of cloning full `Edge` structs into `HashSet` membership. |
| E3 — vector store and HNSW diagnostics | Daemon state carries the vector store explicitly; HNSW metrics include health diagnostics such as average degree, layer distribution, and disconnected node count. |

## Verification Hooks

The relevant focused suites are:

- `cargo test --bin blackboxd workflow::ops`
- `cargo test --bin blackboxd mcp_tools::provenance`
- `cargo test --bin blackboxd project_files`
- `cargo test --bin blackboxd providers`
- `cargo test --bin blackboxd git_history`
- `cargo test --bin blackboxd storage_health`

This document stays as the audit record for why those release-note caveats no
longer block the agentic-corpus implementation.

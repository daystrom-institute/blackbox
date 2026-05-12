# Agentic Corpus — Shipped-Phase Follow-ups

Status: proposed (deferred items captured by release-notes for shipped phases).
Related: `design/archive/agentic-corpus-release-notes.md` (source of every
item below — see archived per-phase section for original context).

## Thesis

The 38 substantive phases of `agentic-corpus-impl` landed. Several
release notes carry explicit deferrals — caveats that were acceptable
at v1 ship but warrant their own design rounds before being treated as
done. This doc consolidates them.

Each item is bounded: it doesn't justify a fresh skeleton, but it does
need a thought-out follow-up before it can be marked closed.

## Follow-ups by phase

### F3 — Schema migration workflow ops

Source: `archive/agentic-corpus-release-notes.md` §F3.

The shipped `schema-migration-arc.json` documents migration shape but
the actual drop+rebuild runs in `TranscriptIndex::open_or_create`.
The workflow's hook ops `schema_migration_drop` /
`schema_migration_rebuild` aren't wired yet. Wire them so the
workflow becomes the runner, not the documentation.

### H3 — Eval drift-policy gate consumes shell output

Source: `archive/agentic-corpus-release-notes.md` §H3.

`nightly-eval-arc.json`'s `Decide` node currently runs against the
workflow's policy entity, not against `RunSuite`'s shell stdout. The
shell harness is the actual runner; the workflow is audit trail.

Unblocks when workflow hook ops grow shell-output capture (e.g.
`op: shell` populating a var with stdout/exit/parsed JSON). At that
point the `Decide` gate routes on real drift verdict.

### P1 — Backfill tool-call edges on project register

Source: `archive/agentic-corpus-release-notes.md` §P1.

Tool-call edges only emit when the touched file is under a registered
project. Registering a project does not retroactively walk transcripts
and backfill. Add a `bbox_project_register` post-step that walks prior
transcripts and emits `EDITED_FILE` / `READ_FILE` / `RAN_BASH` edges
for the newly registered project.

### G2 — `notes.mergeStrategy = union` auto-config

Source: `archive/agentic-corpus-release-notes.md` §G2.

Cross-machine provenance notes need
`git config notes.mergeStrategy union` to avoid silent overwrite when
collaborating machines push to the same notes ref. The daemon
documents this but doesn't write project git config. Decide: do we
auto-set it on first `bbox_provenance_export`, prompt the operator,
or leave it as docs-only?

### M2 — Multi-partition compaction loop / fork

Source: `archive/agentic-corpus-release-notes.md` §M2.

`embed-compaction-arc.json` rebuilds only the single worst-`deleted_ratio`
partition per tick. Additional partitions wait for later cron ticks.
Workflow phase needed: loop inside the arc, or `fork` once the
workflow engine supports fan-out for this use case.

Also: `QuiesceSearch` and `SwapAtomic` are v1 marker hooks (reads
continue serving from the current in-memory snapshot under the
partition lock). If vector search moves out-of-process or serves
concurrently across mutable snapshots, `QuiesceSearch` needs a real
traffic-drain implementation and `SwapAtomic` needs an explicit
publish/rename step.

### M3 — `task-completed` signal from `bro_exec` finalize

Source: `archive/agentic-corpus-release-notes.md` §M3.

`auto-digest-arc.json` is ready to start on `task-completed` events
but the `bro_exec` finalize path doesn't emit them yet. V1 trigger is
manual `bro_orchestrate_run` with seeded `source_session`, `task_kind`,
`daily_count`. Emit the completion signal from bro task finalization
and route it into the installed workflow.

Also: `source_query` isn't reliably populated until the completion
signal carries the originating prompt. The packet accepts any one of
`source_session` / `source_query` / `source_files` for v1. Tighten
once the signal carries query.

### M5 — Auto-edge-extraction nightly trigger + scan cap

Source: `archive/agentic-corpus-release-notes.md` §M5.

For v1, scheduled candidate scan is observable-only and capped at 50;
operator must seed `vars.candidate` manually for a specific candidate
pair. Lift the cap and ship automatic nightly triggering once tier-0
contradiction false-positive rate is measured under real load.

### S4 — EdgeIndex memory model at scale

Source: `archive/agentic-corpus-release-notes.md` §S4.

Dedup uses full `Edge` structs as `HashSet` keys. Fine at current
scale; needs rework if edge count crosses ~5M. Revisit when a corpus
actually exceeds that, OR pre-emptively before Tier B per-language
AST work lands (see `agentic-corpus-tier-b-ast.md` — resolved CALLS
edges may push past the ceiling).

### E3 — VectorStore singleton + HNSW diagnostic metrics

Source: `archive/agentic-corpus-release-notes.md` §E3.

Two items:
- `VectorStore` exposes process-global module functions via `OnceLock`
  for daemon-wiring convenience. Tests and direct callers can use
  explicit `VectorStore::open`. Cleanup: pass `&VectorStore` through
  the embedding queue + search layers and keep the singleton at the
  daemon boundary only.
- HNSW metrics report node counts, dimensions, max level, entry point,
  neighbor refs. They don't report **health diagnostics**: average
  neighbor degree, layer distribution, disconnected-node counts. Add
  before relying on vector metrics for production tuning.

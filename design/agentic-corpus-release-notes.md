# Agentic Corpus Release Notes

## F3

- Tantivy schema version `agentic-corpus-f3` adds agentic-corpus fields and
  drops the derived transcript index on first daemon start after upgrade.
- The background reindexer rebuilds from immutable transcript sources; search
  may report an empty index until that first rebuild commits.
- `schema-migration-arc.json` currently documents the migration shape; actual
  drop+rebuild runs in `TranscriptIndex::open_or_create` until hook ops
  `schema_migration_drop` / `schema_migration_rebuild` are wired in a later
  phase. The workflow + packet are installed via the F4 catalog so the spec
  lives alongside the mechanism.

## F4

- Artifact JSON uses a monotonic integer `version` field. The catalog stores it
  as metadata text so later semver-like strings can be accepted without a
  storage migration.
- Workflows, packets, and brofiles installed through the artifact catalog are
  copied under `$BLACKBOX_STATE_DIR/artifacts/<kind>/<name>.json` with
  `<kind>/<name>/metadata.json` tracking install source and supersession state.

## H3

The shipped `nightly-eval-arc.json` records that the eval shell harness ran but
does not route the actual scoreboard through the `eval/drift-policy` gate. The
`Decide` node's gate runs against the workflow's policy entity, not against
`RunSuite`'s shell output. The shell harness itself is the real runner; the
workflow is documentation and audit trail.

When workflow hook ops grow shell-output capture, such as `op: shell`
populating a var with stdout, exit code, or parsed JSON, the `Decide` gate can
route on actual drift verdict.

## P1

Tool-call edges only emit when the touched file is under a registered project.
Files edited in unregistered projects are recorded in transcripts but produce
no `EDITED_FILE`, `READ_FILE`, or `RAN_BASH` edges. After registering a new
project, no backfill happens; only future tool calls produce edges.

Future improvement: a `bbox_project_register` post-step that walks transcripts
and backfills tool-call edges for the newly registered project.

## G2

Cross-machine provenance is serialized as git notes under
`refs/notes/<namespace>/provenance`. The namespace defaults to `bbox` and can
be changed with `BBOX_GIT_NOTES_NAMESPACE`, for example `bbox-dev` or
`bbox-prod` in multi-instance setups.

`bbox_provenance_export` writes note JSON for commits that have tracked
tool-call anchors. `bbox_provenance_import` reads those notes and replays the
tool-call edges into the local EdgeIndex sidecar after a clone or fetch.

Exports are append-only: each export appends another JSON document to the
commit's provenance note, separated by `--bbox-note-separator--`, so
collaborating machines do not silently overwrite each other's provenance.
Import parses every document in the note and deduplicates replayed edges before
writing the local sidecar.

Operators who share notes across divergent branches should configure the repo
with `git config notes.mergeStrategy union`. The daemon documents this merge
strategy but does not write project git config automatically.

Manual cross-machine flow:
`bbox_provenance_export` → `git push origin 'refs/notes/bbox/*'` → remote
machine runs `git fetch origin 'refs/notes/bbox/*:refs/notes/bbox/*'` →
`bbox_provenance_import`.

## M2

`embed-compaction-arc.json` documents the vector compaction lifecycle:
read vector status, classify deleted-ratio with `embed/compaction-policy`,
quiesce, rebuild HNSW from WAL, and swap. The concrete compaction mechanism is
still `vectors::rebuild(route)`; the workflow is an observable arc around that
existing rebuild path.

The `read_vector_status` hook writes vector metrics into `vars.vector_status`
and the gate packet reads `vars.vector_status.max_deleted_ratio`. This is the
closest current workflow-engine shape to the design's "Decide against
deleted_ratio" intent. The shipped cron spec is an example inlet; cron
installation remains through the existing cron admin/MCP surface, while the
workflow and packets install through the artifact catalog.

## S3

- Code chunking uses `tree-sitter-language-pack` with runtime downloads
  disabled. The repo cargo config sets
  `TSLP_LANGUAGES=rust,python,csharp,java,go,typescript,javascript,c,cpp` so the
  supported parsers are statically compiled into the daemon where parser
  sources are present; direct tree-sitter grammar crates provide the same
  no-download parser subset as a fallback.

## S4

- EdgeIndex memory model: dedup uses full `Edge` structs as `HashSet` keys.
  This is fine at current corpus scale, but needs rework if edge count crosses
  roughly 5M; revisit when corpora actually exceed that scale.

## E3

- VectorStore still exposes process-global module functions via `OnceLock` for
  daemon wiring convenience. Tests and direct callers can use explicit
  `VectorStore::open`; a future cleanup should pass `&VectorStore` through the
  embedding queue and search layers and keep the singleton at the daemon
  boundary.
- HNSW metrics currently report node counts, dimensions, max level, entry
  point, and neighbor references. They do not yet report health diagnostics
  such as average neighbor degree, layer distribution, or disconnected-node
  counts; add those before relying on vector metrics for production tuning.

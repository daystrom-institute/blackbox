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

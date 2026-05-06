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
deleted_ratio" intent. An integration test now verifies that node gates evaluate
against the workflow context's flattened `vars` object, so the compaction
packet can select the `compact` branch from vector-status metrics rather than
falling through to `skip`. The shipped cron spec is an example inlet; cron
installation remains through the existing cron admin/MCP surface, while the
workflow and packets install through the artifact catalog.

`QuiesceSearch` and `SwapAtomic` are v1 marker hooks. Reads continue serving
from the current in-memory partition snapshot while the rebuild path reconstructs
from WAL under the partition lock, then the rebuilt partition state is made
visible by `vectors::rebuild(route)`. If vector search moves out of process or
serves concurrently across multiple mutable snapshots, `QuiesceSearch` must grow
a real traffic-drain implementation and `SwapAtomic` should own the explicit
publish/rename step.

Each compaction-arc tick rebuilds only the single partition with the worst
`deleted_ratio`. Additional partitions above the threshold wait for later cron
ticks. Multi-partition compaction needs a follow-up workflow phase that loops
inside the arc or fans out with `fork` once the workflow engine supports that
shape for this use case.

## M3

`auto-digest-arc.json` is the first agentic-corpus workflow that dispatches an
LLM from the bbox workflow engine itself: the `ProposeEntries` node runs the
`digest-extractor` brofile and expects strict JSON candidates. Hook ops then
parse and validate the candidate shape, gate it through
`auto-digest/entry-quality`, and either apply it through the knowledge MCP
tools, surface it to the side-channel inbox, or log a rejection note.

The `task-completed` routing signal is not emitted by the `bro_exec` finalize
path yet. The installed `auto-digest/task-completed-routing` packet is ready to
start the arc for any `task-completed` event, but for v1 trigger
`auto-digest-arc` manually with `bro_orchestrate_run` or the workflow admin
surface and seed `source_session`, `task_kind`, and `daily_count` in initial
vars. A later phase should emit the completion signal from bro task
finalization and route it into this installed workflow.

The v1 trigger does not reliably populate `source_query`. Until the
`task-completed` signal carries the originating prompt/query, the
`auto-digest/entry-quality` packet treats provenance as present when any one
of `source_session`, `source_query`, or `source_files` is present. Candidates
with all three absent are rejected.

The M2 compaction gate test verified that workflow gate entities include
flattened `vars`, so the auto-digest quality and bro-trust packets now
standardize on `vars.candidate.*` fields rather than dual flat-or-vars
predicates.

## M4

`KnowledgeEntry.links` is now the durable authored-edge surface for reviewed
knowledge relationships. EdgeIndex projects those links on rebuild with
explicit provenance and the stored confidence, while the older
`KnowledgeEntry.supersedes` field continues to project the legacy `SUPERSEDES`
chain.

`contradiction-review-arc.json` uses the engine whiteboard primitive: three
specialist brofiles post and debate independently, a facilitator emits strict
JSON, and `contradiction/review-synthesis` routes the result into
`append_knowledge_link` hook ops. The `supersedes` verdict writes an authored
`SUPERSEDES` link for graph traversal; primary decision supersession should
still use `bbox_decide(supersedes=...)`.

Before running the arc, install the ensemble team with
`examples/agentic-corpus/scripts/install-teams.sh`. It creates
`contradiction-specialists` with the three specialist brofiles. The F4 catalog
installs the brofiles and workflow; teams remain an operator setup step in v1.

Tier-0 contradiction detection runs in the knowledge embedding success path.
When a new knowledge vector has cosine > 0.85 against another knowledge entry
outside the immediate supersession relation, bbox starts
`contradiction-review-arc` if installed; otherwise it emits a
`bbox_note(kind=surprise)` so the unresolved contradiction surfaces in
`bbox_inbox`.

## M5

`auto-edge-arc.json` installs the DESCRIBES / REFERENCES semantic edge review
shape: three read-only classifier brofiles vote independently, the
`auto-edge/vote-aggregate` packet maps the three votes to
`write_edge | hold_for_review | reject`, and `write_semantic_edge` writes the
reviewed edge.

For v1, the scheduled candidate scan is observable-only and capped at 50; seed
`vars.candidate` manually when running the arc for a specific candidate pair.
`REFERENCES` edges append to `KnowledgeEntry.links`; `DESCRIBES` edges are
written to the project EdgeIndex sidecar as explicit heuristic edges. Automatic
nightly triggering is deferred; manual `bbox_orchestrate_run` is the supported
test path.

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

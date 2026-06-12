# bindings/ — the refactor cell-DSL namespaces (`code.*`, `edits.*`, `lsp.*`, `java.*`, `analysis.*`)

Domain bindings projected into code-mode cells as namespace globals. Design
home: `design/bro-harness/refactor-v2-pressure-test.md` (read before
extending); trust model from `design/bro-harness/refactor-tools-v2.md` §3–4;
platform contract from `design/bro-harness/code-mode-cell-dsl.md`.

## Placement invariants

- **Cell-only constructs.** Bindings join the code-mode callable set + seam
  (ToolFilter still gates by canonical dotted name, e.g. `"code.items"`) and
  NEVER the flat wire registry. A model outside a cell cannot call them.
- **Harness-native, zero daemon reach-back** (decision af3c4783): every
  binding is a function of the working set plus harness-owned children
  (language servers). The container test — harness + working set + spawned
  servers, daemon absent — must keep passing. A binding that needs daemon
  state is in the wrong layer.
- **Session-scoped state** (EditStore, LSP pool, provenance ledger) is
  scoped solely by `binding_tools()` being called once per session.
  Construct it twice and EditSets/provenance silently fork. The ledger is
  deliberately shared between producers (`lsp.*`) and consumers
  (`edits.merge`/`apply`) — that shared seam is what makes lineage
  host-computed.

## Two tiers: facts vs analysis (don't conflate them)

- **`code.*` facts** answer "where is X?" for editing — they return
  hash-anchored Spans and are **aggregate-capped**. A multi-file
  `code.query({files})` over a whole repo is bounded
  (`MAX_AGGREGATE_QUERY_CAPTURES`) and reports `aggregate_capped` +
  `files_scanned`/`files_total` + a narrow-it hint. The cap is not a
  nuisance: values-not-refs means every returned value lives in the V8
  isolate heap (V8 default, ~1.5 GB here), and a broad repo-wide query
  flattened past it and OOM'd the isolate — a fatal `FatalProcessOutOfMemory`
  in JSON parse, not a catchable error (gap-fb7a1f99). **Any new
  fan-out/multi-file binding MUST bound its payload to the isolate heap.**
- **`analysis.*`** answers "what is the *structure* of X?" — it runs the
  reduction Rust-side and returns a small structured result (a cluster
  graph, a dependency summary). The raw intermediate (every field touch,
  every call edge) never enters the isolate; the reduced answer is the
  product. Still values-not-refs: the value IS the reduced answer, computed
  where the data lives. Cohesion / dependency / reference-count questions
  belong HERE — never reconstructed in JS from `code.query` captures
  (that path OOMs on a sweep, or burns ~50 cells doing the reduction by
  hand). Two-tier rationale: pressure-test §6.7.

## Porting a v1 planner as a binding (the `lsp.rename` shape, generalized)

`java.*` transforms and `analysis.*` reductions are thin adapters over
`bbox_refactor::plan`: run the v1 analysis/synthesis verbatim, strip the
MCP/plan-apply envelope, return data for the edits algebra (transforms:
`{changes, creates, findings}`) or a reduced structure (analysis). They
NEVER write; findings are the v1 structs verbatim, re-keyed under one array.
Footguns that bit:

- **Planner-emitted NEW files arrive as whole-content `0..0` edits against
  the empty-file hash** (v1's create idiom — its apply created missing
  files). Convert them to `creates` (→ `edits.createFile`), or the algebra
  stale_span-bounces them against a file that does not exist yet.
- **Transforms are NOT idempotent over their own output.** A re-call after a
  successful apply hits the planner's target-exists refusal — that is the
  DONE signal, not a retry. Without that framing an agent shell-deletes the
  created file and loops; the error and the describe contract both name it.
- **Wide toolboxes stay a compact index.** The namespace description is one
  line per transform; `java.describe`/`analysis.describe` returns the full
  contract at runtime (values stay in the isolate — no MCP cap, no
  exec-prompt bloat). Do not inline N signatures into the description.
- **Extract-to-delegate transforms preserve the public API on demand.**
  `java.extractClass`'s `wrappers` leaves delegating stubs on the source so
  external callers compile unchanged. Survey callers first (the one-call
  `code.files` → `code.query({files})`); the transform can't know whether
  anything off-file uses a moved method.

## The trust model (don't re-litigate per binding)

- **One mutation path.** `edits.apply` is the only binding that writes.
  Everything else returns data: facts return Spans, authorities return
  `{changes, findings}` for `edits.merge`. New mutating capability routes
  through the existing choke point, not a second writer.
- **No confirm flags, ever.** A confirm/ack a cell can author is theater.
  The gate is detection — stale_span, invalid_edits, create_exists,
  parse_error_after_apply — bouncing with `applied: false` + findings
  `{kind, file, detail, resolution_hint}` and byte-exact rollback. Findings
  must be repairable without re-running discovery. Operator authority
  (RX-V1 flags) arrives dispatch-side, never as a cell argument.
- **Spans are hash-anchored at read time**; an EditSet pins one content
  hash per file; after a successful apply every older Span for that file is
  stale BY DESIGN — re-derive facts. Check expected hashes BEFORE
  interpreting byte ranges: drift must fail as `stale_span`, never as a
  confusing structural miss against the new tree.
- **Provenance is host-computed lineage**, never cell-supplied tags. The
  choke point recomputes `semantic_status` as the weakest link across edit
  producers; cell-authored bytes floor at `syntax_only`; laundering is
  possible and priced, not forbidden.
- **LSP fails closed (RX-V3).** `lsp_unavailable` is an error; never a
  silent downgrade to text matching. Selection does not poison lineage:
  spans used to AIM an authority don't enter the lineage of the edits that
  authority produces.

## Probe-derived shapes (look arbitrary without this history)

Binding ergonomics evolve only through live probes +
`prompts/RETRO_ISOLATE_REFACTOR.md` retros; fix at the source (binding >
declarations > gap note in `*/refactor-tools/*` dedupe namespace). Shapes
that exist because a real agent burned cells without them:

- `edits.begin()` returns a bare id string (agents passed the `{es}`
  wrapper object onward when it returned an object).
- Lenient input normalization (es-as-wrapper, span-as-JSON-string) with
  decode errors that name the expected shape.
- `lsp.rename` snaps whole-item spans to the name identifier (item spans
  start at `pub`, which the server refuses).
- `code.items` carries `visibility` and `source_len` (their absence misled
  agents into abandoning the namespace).
- `Applied` carries the new generation's `content_sha256` per file (saves a
  re-inventory round-trip before follow-up edits).

Do not "clean these up" toward purity without re-probing.

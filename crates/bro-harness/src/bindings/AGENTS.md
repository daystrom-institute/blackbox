# bindings/ — the refactor cell-DSL namespaces (`code.*`, `edits.*`, `lsp.*`, `java.*`, `analysis.*`)

Domain bindings projected into code-mode cells as namespace globals. Design
home: `design/bro-harness/refactor-v2-pressure-test.md` (read before
extending); trust model from `design/bro-harness/refactor-tools-v2.md` §3–4;
platform contract from `design/bro-harness/code-mode-cell-dsl.md`.

## Placement invariants

- **Cell-only constructs.** Bindings join the code-mode callable set + seam
  (ToolFilter still gates by canonical dotted name, e.g. `"code.items"`) and
  NEVER the flat wire registry. A model outside a cell cannot call them.
- **Invocation form: bare namespace globals, never `tools.<ns>.*`.**
  Namespace-bound bindings project as top-level globals (`rust.migrateErrorType(...)`,
  `code.items(...)`); the flat `tools.*` object holds only unbound core tools
  (file/shell/git/web). `tools.rust` is undefined and calling it fails with
  `TypeError: Cannot read properties of undefined`: three probe dispatches
  burned on exactly this before the error message was read properly. When a
  probe agent claims a binding "does not exist", check its invocation form
  before believing the tool is missing.
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
- **`analysis.references` is the count-mode answer for Java usage surveys.**
  Use it when the agent needs per-symbol reference counts, file lists, and
  a few examples to choose `wrappers`, estimate blast radius, or distinguish
  real concern-private state from forwarded fields. It deliberately returns
  no full `usages` array and no hash-anchored spans; if the next step needs
  edit addresses, re-derive them with `code.*`.

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
  external callers compile unchanged. Survey callers first with
  `analysis.references({symbols: seam.item_names, kinds:["method_invocation"]})`;
  the transform can't know whether
  anything off-file uses a moved method.
- **DI policy lives in the binding, never the engine.** `bbox_refactor`'s
  extract synthesis is framework-neutral by charter; the binding is the layer
  that reads the source and decides wiring. `java.extractClass` auto-defaults a
  Guice/DI source (uses `@Inject`) to `external_injection` so the delegate is a
  container-constructed `@Inject` bean — interceptable by Guice AOP, which a
  `new`-ed (`own_construction`) delegate is not. The delegate is left UNSCOPED
  (never `@Singleton`): Guice JIT-binds a concrete `@Inject` class fresh per
  injection point, matching a view's per-instance lifecycle so moved mutable
  state can't leak. The `@Inject` flavor is matched to the source
  (`com.google.inject` / `jakarta` / `javax`). Leave `wiring` unset to get this.
- **`expected_wiring` ≠ `wiring` — the vocabulary collision that bit a probe.**
  `analysis.cohesionClusters`'s `expected_wiring`
  (`delegate`/`callback`/`source_instance`) is a cohesion-TOPOLOGY / seam-quality
  signal; `java.extractClass`'s `wiring`
  (`own_construction`/`external_injection`/`none`) is the DI STRATEGY. Feeding
  the former into the latter (a recipe once did) makes the agent pass an invalid
  enum and "repair" it to `own_construction`, silently defeating the AOP-ready
  default. The cohesion recipe leaves `wiring` unset.
- **Moving the injection point composes; it is not an extract side-effect.**
  `external_injection` leaves the moved deps as dead `@Inject` params on the
  source ctor. `java.removeUnusedConstructorParams` drops them, but only AFTER
  the extract is applied — the orphaned `this.dep = dep` must already be gone or
  the param still reads as referenced. Flow:
  `extractClass → apply → removeUnusedConstructorParams → apply`.

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
  (RX-V1 flags) arrives dispatch-side, never as a cell argument: the binding
  reads it host-side via `cx.tool_arg_defaults.lookup(tool, param)`, and the
  daemon fills that map from ambient context, then the brofile's
  `tool_defaults`, then per-dispatch `ExecParams.tool_defaults` (most
  specific wins), forwarded to the harness child via `--additional-context`.
  A cell-authored `acknowledge_*` is a schema error naming the channel.
  Isolate probes exercise the granted leg with `--tool-defaults`.
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

# bindings/ — the refactor cell-DSL namespaces (`code.*`, `edits.*`, `lsp.*`)

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

# bbox-refactor — the refactor substrate (two consumers, one future)

Daemon-independent by invariant: dependencies point down (corpus-core,
chunker, bbox-lsp, external) and never at `blackbox`. Two consumers share
this engine, deliberately without a behavioral fork:

- The daemon's v1 MCP adapters (`bbox_refactor_*` plan kinds, refactor runs)
  — **legacy, on the kill list** (decision af3c4783; refactor-tools-v2 §7
  strangler). Keep them working; do not grow them. The 100+ plan-kind
  catalog is CLOSED: new capability does not get a new `kind`.
- bro-harness cell bindings (`code.*`/`edits.*`/`lsp.*`/`java.*`/`analysis.*`)
  — the future. New capability lands as pure, data-returning substrate
  functions (the `facts` module pattern), changes-returning transforms
  (`java.*`, consumed by `edits.merge`), or **reductions** (`analysis.*`:
  run the graph/analysis Rust-side, return a small structured answer — the
  raw edges never cross into the cell). All three are thin adapters over
  `plan`; none of them gets a new `kind`.

## The facts charter (load-bearing for the container test)

`facts` is pure functions of file bytes: no LSP, no daemon state, **no
writes**. The harness-native invariant — put the harness in a container and
refactor still works with the daemon absent — depends on this module staying
that way. Write machinery lives separately (`write_atomic` /
`write_atomic_noclobber` / `apply_text_edits`), kept verbatim from v1 because
the cell choke point reuses it.

## Footguns with history

- **Hash-check ordering**: when a caller supplies an expected content sha,
  verify it BEFORE interpreting byte ranges against a fresh parse. A drifted
  file must fail as `stale_span`; checked after, it surfaces as a confusing
  structural miss ("no function_item at range") against the new tree. This
  bit during development; the ordering is now part of the function contracts.
- **noclobber semantics**: `write_atomic_noclobber` closes the create TOCTOU
  at the rename. On its failure the target was never touched — callers must
  NOT push it into their rollback list.
- Grammars come from bbox-chunker (`ts_language_for_name`); item inventory
  walks are per-language (rust/java/generic fallback). `ParsedSource` is
  crate-private on purpose: expose derived data, never parse state.
- tree-sitter query iteration needs `streaming_iterator::StreamingIterator`
  — the cursor's matches are not a std Iterator.
- **Constructors are noise in cohesion / field-graph reductions.** The class
  walker counts a constructor as a method, and a constructor assigns ~every
  field, so its `method_to_field` edges transitively merge every concern into
  one cluster — fatal on the god classes `cohesive_clusters` exists to split.
  It now drops methods whose name == the class name before partitioning. Any
  new reduction over the method/field graph must decide whether constructors
  are signal; for cohesion they are not. (Connector-aware refinement landed,
  gap-2a3f03e5: partitioning is no longer transitive closure but modularity
  community detection over an inverse-field-frequency-weighted graph — a
  field's per-pair weight is `1/(deg-1)`, so a high-fan-out bridge field
  contributes diffuse weak edges and can no longer fuse distinct concerns.
  Determinism preserved: sorted node visitation + incumbent-then-smallest-id
  tie-break. `MODULARITY_RESOLUTION` is the future finer-seam knob.)
- **Constructor-body inserts obey the `super()`/`this()` first-statement rule
  (JLS 8.8.7).** `constructor_body_insert_position` anchors AFTER a leading
  `explicit_constructor_invocation` when present; inserting delegate-wiring
  before it makes the call no longer first, produces a tree-sitter error
  node, and the cell choke point bounces the apply on post-write validation.
  Synthetic fixtures without `super()` hide this — it bit only on a real
  class whose constructor began with `super()`.
- **Moved DI fields thread through the target ctor; the parameter restriction
  is load-bearing (gap-9462575f).** `extract_class` threads a moved field that
  the source ctor initializes *from a surviving ctor parameter*
  (`this.repo = repo`, incl. the field/param name-mismatch case) into the
  target's constructor — target ctor param + assignment, the param passed at
  the source-side construction, the orphaned source assignment deleted. The
  restriction to bare-ctor-parameter initializers is what keeps it safe: an
  initializer that is a method call or computed expr (`this.grid = buildGrid()`
  where `buildGrid` is moved) is NOT threaded, and only genuine injected deps —
  never mutable state or constants — become ctor params. A single-line ctor
  body (`X(Repo r) { this.r = r; }`) shares its line with the signature, so the
  orphan deletion must be statement-scoped there, not full-line, or it eats the
  signature and collides with the wiring insert.
- **`removeUnusedConstructorParams` (`prune_ctor_params.rs`) is the composable
  injection-point move, not an extract side-effect.** After an extract strands
  a dependency's ctor parameter, this pure substrate fn drops params with zero
  references in the `@Inject` ctor *body* — a parameter is ctor-scoped, so
  "unused" is a local decision (no whole-class scan), and reference-counting
  deliberately over-counts (a same-named field access keeps the param) so the
  ambiguous direction is never an unsafe delete. `@Inject` ctors ONLY: a
  manually-called ctor's `new` callers would break. One change replaces the
  whole parameter list (no per-param comma surgery). New capability, no new plan
  kind — a pure fn exposed at the crate root behind a `java.*` binding.

## Sibling boundary

`bbox-macros` (the declarative macro layer) is folding down into cell
recipes + the function store (pressure-test doc §6.6; kill-list formalized
in decision b8dc263d — `macro_*` + the planner/registry/expr DSL retire at
parity). Its Java emission sidecar is the salvage. Don't add new coupling
from here to there.

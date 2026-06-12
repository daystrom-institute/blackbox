---
title: "Refactor v2 pressure test: three kinds as cell programs"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - refactor-tools
brief: "The §5 spike from refactor-tools-v2.md, executed: hand-written cell programs for rust_lsp_rename / extract_rust_trait / extract_java_class against a hypothetical binding surface, so the minimal binding set falls out of working programs instead of paper design. Also records the grounding deltas between the v2/cell-dsl docs and the code at 65420f2 (refactor_plan capability tools already shipped daemon-side; namespace description machinery exists but only renders in code_mode=only; bbox-refactor drags bbox-lsp+rmcp; flat tools object only). Ends with the v0 binding set, the four decisions the programs force, and the live-probe + tailored-retro validation plan."
---

# Refactor v2 pressure test: three kinds as cell programs

> **Status: working spike, not ratified design.** This executes the first
> step [`refactor-tools-v2.md`](./refactor-tools-v2.md) §5 prescribes —
> "hand-write the cell program for three kinds spanning the difficulty range,
> before building anything" — and grounds it against the code as of
> `65420f2`, which has moved under the v2/cell-dsl docs in specific ways
> (§1). The programs target a **hypothetical** binding surface; none of
> `code.*`/`lsp.*`/`analysis.*`/`edits.*` exists yet. Validation is
> live-probe-driven (§6): patterns earn their place by what dispatched
> agents can actually use, with retrospective friction reports, not by
> doc fiat.

## 1. Grounding deltas: where the code moved under the docs

Verified at `65420f2` (branch `beta/blackbox-v2`):

- **D1 — a ref-handle refactor capability already shipped, daemon-backed.**
  `bro_capabilities::RefactorCapability` (`crates/bro-capabilities/src/lib.rs:93-103`)
  with `plan_refactor() -> RefactorPlanHandle {id, preview}` /
  `materialize_plan(id)`, implemented by `DaemonRefactor`
  (`src/orchestration/capabilities.rs:116`) as a thin wrapper over the **v1**
  union dispatch (`refactor::plan_with_ctx`, same `RefactorPlanParams`, plans
  held in a daemon-side handle store as `ref:plan/<uuid>`). Exposed as flat
  harness tools `refactor_plan` / `refactor_plan_get`
  (`crates/bro-harness/src/capabilities.rs:152-154`), which project into
  cells as `tools.refactor_plan(...)` like any other tool. The v2 doc's "no
  refactor bindings in-box" is stale; what exists is the *interim
  ref-handle pattern* — daemon-resident state, plan-kind union params — that
  both v2 (§2 values-not-refs) and cell-dsl (§1 "daemon appears nowhere")
  argue against as the end state. Disposition (operator, 2026-06-12):
  **retire, no bridge.** The capability path is daemon-resident state and
  fails the end-state invariant below; harness-native bindings replace it,
  they do not wrap it. Kill list at parity: `DaemonRefactor` +
  `install_refactor`, the `RefactorCapability` trait itself (it exists only
  to inject the daemon impl), the `refactor_plan`/`refactor_plan_get` flat
  tools, and the `bbox_refactor_*`/`bbox_slice_*` MCP adapters per v2 §7.
- **D2 — namespace machinery half-exists, description-side only.**
  `ToolNamespaceDescription` + namespace grouping live in
  `bro-code-mode/src/description.rs` (`build_exec_tool_description`,
  `ToolName::namespaced`), but the harness passes an **empty** namespace map
  (`bro-harness/src/code_mode.rs:454`), and the isolate-side `tools` object
  is flat (`runtime/globals.rs:47-63` — one property per `global_name`).
  Nested namespace *objects* (`code.items(...)` as a global) need a runtime
  mechanism that does not exist yet. The cell-dsl §8 "zero runtime changes"
  tenant test applies to domains joining *after* that mechanism lands; the
  mechanism itself is platform work.
- **D3 — TS declarations render only in `code_mode=only`.**
  `build_exec_tool_description` returns early before the per-tool TS catalog
  when `code_mode_only=false` (`description.rs:266-268`), and the harness
  default is `CodeMode::Optional` (`code_mode.rs:39-45`). In the default
  configuration a cell author gets **no typed declarations at all** — only
  the generic exec template plus `ALL_TOOLS` name/description pairs at
  runtime. Binding discoverability (v2 §8's named tension) is therefore
  gated on dispatch configuration today; probes must control for it.
- **D4 — linkage is legal but lopsided.** `bbox-refactor` is
  daemon-independent (deps: `bbox-corpus-core`, `bbox-chunker`, `bbox-lsp` +
  external — its Cargo.toml states the invariant), so
  `bro-harness → bbox-refactor` violates nothing. But it drags `bbox-lsp`
  (the daemon-side multi-language session manager) and `rmcp` (schemars
  derives on param structs only) into the harness, while the harness already
  links `bro-lsp` (Rust-analyzer-only, `crates/bro-lsp`). The v2 §8 "two
  warm pools" tension is already physically true in-process today — the
  daemon links both stacks — but a harness-side binding crate forces the
  choice explicitly.
- **D5 — the empirical baseline already exists.** Gap cluster
  gap-374fba64 / gap-5d17610b / gap-de62454a / gap-903f6949 (filed
  2026-06-12, session `da082ef9`) is a live-probe record of driving the v1
  MCP surface from cells: union schema unusable, catalog over the 81920-byte
  MCP cap, no cross-isolate continuity, authorable `confirm=true`. These are
  the friction findings any v2 binding set must dissolve; the retro loop in
  §6 measures against them.

### The end-state invariant (operator-set, 2026-06-12)

**Refactor tooling is harness-native: put the harness in a container and
every binding still works with zero daemon reach-back.** The falsifiable
form: a `bro-harness` process with only its working set, its own spawned
language servers, and the v0 bindings linked in must run all three programs
in §2–§4 with the daemon absent. Consequences:

- All refactor state is harness-local: EditSets, the provenance ledger,
  LSP sessions (harness-spawned children next to the files — `bro-lsp`
  grown, or `bbox-lsp`'s manager linked harness-side; never the daemon's
  warm pool), validation runs, snapshots/rollback.
- The daemon keeps exactly its two boundary roles
  (remote-worker-boundary §4): dispatch composition (which namespaces a
  cell sees) and integration (what artifacts re-enter canonical state).
  Nothing refactor-shaped crosses at exec time.
- The external MCP refactor surface dies at parity (v2 §7); MCP-only
  consumers get refactoring by directing a harness worker or consuming an
  atom whose implementation dispatches the cell path.

## 2. Program 1 — `rust_lsp_rename` (trivial tier)

Validates: the choke point, lineage stamping, selection-vs-production
semantics.

```js
// Rename Orchestrator::spawn_task -> spawn_harness_task, workspace-wide.
const hits = await code.query("src/orchestration/mod.rs",
  "(function_item name: (identifier) @n)");
const at = hits.find(h => h.text === "spawn_task");      // judgment at authoring time
const ws = await lsp.rename(at.span, "spawn_harness_task"); // WorkspaceEdit, ledgered lsp_verified

const es = await edits.begin();
await es.merge(ws);
const r = await apply(es, {
  validations: ["tree_sitter_no_errors", { compiler_check: "cargo check" }],
});
text(r.applied ? r.summary : JSON.stringify(r.findings));
```

What fell out:

- **Selection does not poison lineage.** The span that *chose where* to
  rename came from `code.query` (syntax tier), but every edit byte was
  authored by the language server. Lineage tracks the **producer of the
  edits**, not the selector — this program stamps `lsp_verified`. Decide
  this semantic now; it is the difference between lineage being useful and
  every real program flooring at `syntax_only`. (Consistent with v2 §2:
  authorship is adjudication; selection judgment was exercised at
  policy-time.)
- `lsp.rename` takes a Span (byte-anchored), converts to LSP positions at
  the binding edge (cell-dsl §3).
- RX-V3 carries verbatim: no rust-analyzer → `lsp.rename` throws; the cell
  fails loudly, never downgrades.

## 3. Program 2 — `extract_rust_trait` (mid tier)

Validates: the facts/algebra split, batching, where pure-JS templating
starts leaking reproducibility.

```js
const file = "src/orchestration/mod.rs";
const items = await code.items(file);                    // v1 refactor_status inventory, Spans
const imp = items.find(i => i.kind === "impl_item" && i.name === "Orchestrator");
const methods = (await code.query(file, IMPL_FN_QUERY, { within: imp.span }))
  .filter(m => ["dispatch", "cancel", "steer"].includes(m.name));

// One park, batched resolution (cell-dsl §6).
const sigs = await Promise.all(methods.map(m => analysis.fnSignature(m.span)));

const traitSrc = renderTrait("Dispatch", sigs, { vis: "pub" }); // plain JS templating
const es = await edits.begin();
await es.createFile("src/orchestration/dispatch_trait.rs", traitSrc);
await es.replace(imp.headerSpan, "impl Dispatch for Orchestrator");
await es.insertAfter(items.modDecls.at(-1).span, "pub mod dispatch_trait;");
const r = await apply(es, { validations: [{ compiler_check: "cargo check" }] });
```

What fell out:

- `analysis.fnSignature(span)` — or `code.items` returning signature-rich
  items. Either way, signature extraction is a **fact binding**, not a JS
  regex exercise (v2 §5's rule: 300 lines of JS re-deriving analysis means a
  missing binding).
- The algebra needs `insertAfter` and sub-addressing (`headerSpan` on
  items) beyond v2 §3.2's `replace/moveSpan/createFile/merge`.
- `renderTrait` — pure JS templating — is where reproducibility leaks
  (v2 §5's honest trade). This is the first **library-script** candidate,
  not a binding: it has no semantic authority to encode.
- `moveSpan` never got used: create + replace + delete compose the same
  effect. Defer it; sugar can come back if probes want it.

## 4. Program 3 — `extract_java_class` (worst case)

Validates: the findings vocabulary, the bounce loop, host-side rewriting
where wiring is too semantic for cell JS.

```js
const file = "src/main/java/com/acme/OrderService.java";
const items = await code.items(file);
const moved = items.filter(i => MOVE.includes(i.name));
const region = code.spanUnion(moved.map(m => m.span));

const [captures, externals, inherited] = await Promise.all([
  analysis.captures(region),        // CapturedVariable: mutability/static-final classified
  analysis.externalCalls(region),   // ExternalCall: recommended_resolution attached
  analysis.inheritedDeps(region),
]);

// Policy authored here, in the open: final fields ride the constructor;
// mutable captures are left for detection to bounce if unresolved.
const ctorParams = captures.filter(c => !c.source_mutable && !c.source_static_final);

const es = await edits.begin();
await es.createFile(TARGET, renderClass("OrderPricing", moved, ctorParams, externals));
for (const m of moved) await es.delete(m.span);
await es.merge(await analysis.rewireAccessors(region, {
  target: "OrderPricing", wiring: "constructor",
}));                                 // host-authored rewrite — the hard 20% stays Rust

let r = await apply(es, { validations: ["tree_sitter_no_errors"] });
if (!r.applied) {
  // Findings carry span + classification + candidate resolutions (v2 §4):
  // repairable without re-running discovery.
  const mutables = r.findings.filter(f => f.kind === "mutable_capture");
  store("bounced", { findings: r.findings, editSet: es.id });
  text(`bounced: ${r.findings.length} findings, ${mutables.length} mutable captures`);
}
```

What fell out:

- `code.spanUnion`, `es.delete` — trivial additions, but the algebra is now
  begin/replace/insertAfter/delete/createFile/merge: six verbs, no more.
- `analysis.rewireAccessors` — accessor rewiring under a wiring strategy is
  exactly v2 §7's "salvage before deleting": the `extract_java_class`
  planner's wiring/deep-analysis machinery extracted as a fact-and-edit
  binding rather than ported as JS. It *returns edits* (a WorkspaceEdit-like
  ledgered value), it does not write.
- Detection at `apply()` is unconditional: the v1 structs
  (`CapturedVariable` with `source_mutable`/`source_static_final`,
  `bbox-refactor/src/lib.rs:169-183`; `ExternalCall.recommended_resolution`)
  are already the findings vocabulary — they need re-triggering (always-on at
  the choke point), not redesign.
- The bounce stores its EditSet id + findings in the KV (`store`) — the
  repair cell `load`s and continues. This is the cross-isolate continuity
  gap-de62454a wanted, answered with session KV + host-side EditSet state,
  no daemon handle.

## 5. The v0 binding set that fell out

Facts (pure, syntax tier): `code.items(file)`, `code.query(file, q, {within?})`,
`code.read(span)`, `code.spanUnion(spans)`.
Facts (session-backed, LSP tier): `lsp.rename(span, newName)` — Rust first via
`bro-lsp`; `refs`/`codeActions`/`resolve` deferred until a probe wants them.
Facts (analysis tier, Rust-implemented): `analysis.fnSignature(span)`,
`analysis.captures(span)`, `analysis.externalCalls(span)`,
`analysis.inheritedDeps(span)`, `analysis.rewireAccessors(span, opts)`.
Algebra (host-backed builder): `edits.begin() -> EditSet`;
`replace / insertAfter / delete / createFile / merge`.
Choke point: `apply(es, {validations})` → `{applied, summary, semantic_status}`
| `{applied: false, findings}`.

Deliberately absent from v0: `moveSpan` (composable), pagination/catalog
surfaces (no catalog exists to paginate), any confirm flag (v2 §3.3), any
per-kind schema (the kinds are programs now — gap-374fba64 and gap-5d17610b
dissolve rather than resolve).

## 6. Decisions the programs force (open, for operator review)

1. **Linkage strategy.** Bindings live harness-side (NOT in `bro-code-mode`,
   which stays zero-dependency vendored runtime), and the selection
   criterion is the end-state invariant: whatever is linked must run
   container-resident with no daemon process. Both options pass it on
   compile-DAG grounds — `bbox-lsp` is a library that spawns servers
   wherever it's linked; daemon residency of LSP today is a *call-site*
   fact (`DaemonRefactor` feeding `state.lsp_sessions`), not a crate fact.
   Options: (a) new `bro-refactor-bindings` crate linking `bbox-refactor`
   as-is — fastest; drags `bbox-lsp` + `rmcp` into the harness DAG, and the
   harness owns the spawned servers (the daemon's warm pool is never
   touched); (b) peel a `bbox-refactor-core` (analysis structs + edit
   algebra + apply machinery, no LSP manager, no rmcp) — cleaner, one
   extract-crate refactor first. Lean (a)-then-(b): probe on (a), peel once
   the binding surface stabilizes; either way the harness/daemon LSP-pool
   duplication ends in the harness's favor for working-set claims.
2. **Namespace projection mechanism.** Extend the runtime so a
   `ToolDefinition` can declare a namespace and `install_globals` builds
   nested objects (`code`, `lsp`, `analysis`, `edits` as globals beside
   `tools`), plus hand-authored TS declaration blocks composed into the exec
   description (the `ToolNamespaceDescription` slot the harness currently
   feeds an empty map). One runtime change, then domains ship with zero
   runtime changes (cell-dsl §8).
3. **Host-backed EditSet.** The ledger (cell-dsl §4) requires the host to
   watch composition, so `edits.*` are host calls against per-session
   EditSet state keyed by id — chatty, but batched at quiescent boundaries.
   Pure-JS algebra would be quieter and ledger-blind; rejected.
4. **Lineage semantics: producer, not selector** (Program 1). Spans used to
   *aim* a binding do not enter the lineage of the edits that binding
   produces. Without this, `lsp_verified` is unreachable from any real
   program.

   **Disposition (decisions 3–4 built, 2026-06-12).** The provenance ledger
   shipped harness-side (`crates/bro-harness/src/bindings/ledger.rs`) on the
   merge/apply seam: `lsp.rename` records each returned `{span, new_text}`
   change at `lsp_verified` under an issuance id; `edits.merge` recognizes
   consumed changes and stores per-edit tiers in the EditSet; `edits.apply`
   recomputes the set's `semantic_status` as the weakest link (file creates
   are cell-authored) and reports a `lineage` breakdown. Recognition is **by
   canonical content digest, not by id** — the cell idiom passes `r.changes`
   bare through filters/spreads/JSON round-trips, so cell-dsl §9's "digest
   re-matching backstop" is the primary mechanism by construction: per-change
   keying means a filtered subset keeps its provenance, and a hand-rewritten
   change silently floors at `syntax_only` (laundering priced, not
   forbidden). The issuance id rides the producer envelope for correlation
   only. The ledger is session-scoped beside the EditStore, so
   `store()`/`load()` continuity across cells is free — cell-dsl §4's
   `recalled` mark turned out unnecessary.

   *Probe-validated (probe-ledger-1, 2026-06-12):* a GLM agent given the
   standard cross-file rename task produced an apply result of
   `lineage {lsp_verified: 2, syntax_only: 0}`,
   `semantic_status: "lsp_verified"` — read from the raw tool result, not
   agent paraphrase — in 7 turns with zero errored cells (parity with the
   best prior lsp probe; the ledger added no friction). Program 1's
   producer-not-selector claim now holds end-to-end: `lsp_verified` is
   reachable from a real dispatched program.

## 6.5 The mechanical toolbox: transform bindings (the lsp.rename shape, generalized)

The v1 Java catalog (~40 kinds, ~38 modules, ~800KB of Rust: the
`extract_java_class` / jOOQ repository / Vaadin component families) is not
algebra and not library-script material — each is a hard Rust analysis plus
templated edit synthesis. v2 §5's rule decides their form: a transform whose
honest JS port is "300 lines re-deriving capture analysis" is a **binding**.
The probe-validated shape for that binding already exists: `lsp.rename` —
**an authority that returns hash-anchored `{changes, findings}` for
`edits.merge`, and never writes.**

So the toolbox exposes as **transform bindings**:

```ts
const r = await java.extractClass({ file, classes: [...], wiring: "constructor" });
// r: { changes: SpanChange[], findings: Finding[], fixme_count: number }
await edits.merge({ es, changes: r.changes });
await edits.apply({ es });   // same choke point, same detections, same bounce
```

The v1 planners already compute exactly this (FileEdits + capture/external-
call findings) — porting strips the MCP envelope and the plan/apply
orchestration, keeps the analysis and synthesis verbatim. Selection (which
class, which fields) moves to the cell; `deep_analysis`-style flags die
(detection is always-on at the choke point); operator-authority flags stay
dispatch-supplied (RX-V1 relocated, v2 §3.3).

**Surface economics — the part that must not recreate the catalog problem.**
Forty hand-authored TS signatures would bloat the exec description (cell-dsl
§9; render-hygiene rule: deep docs belong in system memories). Mechanism:

1. The `java.*` (and `jooq.*`/`vaadin.*`) namespace descriptions stay a
   compact index — one line per transform, name + purpose.
2. Depth on demand: `java.describe({ transform })` returns the full contract
   (params, findings vocabulary, an example) at runtime — gap-374fba64's
   per-kind-describe ask, legitimately re-emerging for a wide toolbox, but
   as one namespace method with values staying in the isolate (no MCP
   response cap), backed by the same per-language system memory the
   sm-tree-sitter direction prescribes for query grammar.

**Triage before porting (v2 §7, used-kind parity).** Mine transcripts/atom
invocations for which kinds were actually called, then bucket: (a)
expressible today as a short cell program over `code.*`/`edits.*` → library
script; (b) hard analysis/synthesis → transform binding (the extract-class
and jOOQ-repository families are certain members); (c) campaign-bound,
nobody-will-miss-it → not ported (plausibly several Vaadin/jOOQ audits).
"Lombokifier"-style recipes are compositions over (b) plus selection —
library scripts or atoms, not new bindings.

**Gates.** Tree-sitter-backed transforms port now (the java modules are
daemon-independent; grammar via bbox-chunker). `rename_java_symbol` /
`java_lsp_organize_imports` wait on `bro-lsp` growing jdtls (v2 §7's named
gate). External/MCP-only consumers: see §6.6 — the atom tier is demoted;
dispatch-a-worker-with-a-recipe is the interface.

**Pilot.** One real, used transform first — `java_jooq_extract_repository`
or `extract_java_class` — ported as a transform binding with the compact
index + describe, probed on a Java fixture with the standard loop, before
committing to the sweep.

**Disposition (pilot built + probe-validated, 2026-06-12).** Two grounding
corrections from the triage, then the pilot:

- *Used-kind parity is the empty set.* Mining the transcript corpus for
  real `bbox_refactor_plan` invocations found only Rust analysis kinds
  (`rust_impl_partition_analysis`, `rust_top_level_dependency_analysis`,
  `inline_mod_to_file_submodule`); every Java-kind hit is development
  exhaust (building/testing the kinds, authoring atom JSON). This
  corroborates §6.6's empty-registry signal: the sweep's justification is
  *exposure for future consumption*, not parity with past use — which
  strengthens bucket (c) and means most of the catalog should wait for a
  real Java campaign to pull it.
- *`java_jooq_extract_repository` was never a candidate*: its v1 planner
  is a deliberately Blocked stub (refuses repository-scale extraction,
  `PlanStatus::Blocked`, no edits). The pilot is `extract_java_class`,
  the §4 worst case.

The pilot (`crates/bro-harness/src/bindings/java_transforms.rs`):
`java.extractClass` is a thin adapter over the v1 planner verbatim
(`bbox_refactor::plan`, no LSP ctx) returning `{changes, creates,
findings, fixme_count}` — planner-emitted new files (whole-content edits
against the empty hash, v1's create idiom) convert to `creates` for
`edits.createFile`; findings are the v1 analysis structs verbatim,
re-keyed under a `finding` tag; refusals (e.g. `mutable_capture_with_write`)
pass through as operator-actionable errors. Surface economics held:
the namespace description is a one-line-per-transform index and
`java.describe({transform})` returns the full contract in-isolate.
Tree-sitter authority ⇒ `syntax_only` tier; no ledger issuance (the floor
needs no record); Java `lsp_verified` waits on jdtls as gated above.
*Probe-java-1 (GLM, standard loop):* 5 turns, zero errored cells — the
agent batched `java.describe` + `code.items` in one cell, followed the
contract's recipe verbatim (creates → merge → apply), and reported the
choke point's `syntax_only` correctly. Cleanest probe of the series;
describe-on-demand discoverability needed no nudge.

## 6.6 Macros fold down into recipes; the refactor-atom tier demotes

**What `macro_*` is.** The macro synthesis layer
(`design/refactor-tools/unified-code-synthesis-model.md`, lifecycle:
partial; `crates/bbox-macros`, ~800KB incl. a 163K planner) is a
**declarative recipe language**: `MacroDefinition` = versioned id +
`inputs_schema` + probe slots + a fixed operation list (Probe/Emit/Rewrite
via a JavaPoet-grade Java sidecar) + refusal predicates + validations +
RX-V1 authority gates, with a bounded expression DSL (`expr.rs`: dotted-path
context, predicates, `${path}` interpolation) gluing the steps. It is v2
§1's diagnosis recurring one level up from `bbox_refactor_run`: probes are
variables, the expr DSL is JS-badly, refusals are if-statements, the
plan/apply two-phase is the choke point — **a programming language built
because the real one didn't exist yet**. The corroborating signal: the
shipped registry is EMPTY on the prod host — the machinery landed, the
catalog never populated.

**The fold-down mapping:**

| Macro layer piece | Isolate successor |
|---|---|
| Probe slots → named context | `code.*`/`lsp.*` calls into JS variables |
| `expr` predicate/interpolation DSL | plain JavaScript |
| Refusal rules | early returns + the apply bounce |
| Emit/Rewrite operations (Java sidecar) | **survives**: transform/emit bindings (§6.5 authorities returning changes) |
| plan → apply two-phase | EditSet build → `edits.apply` (detections, no confirm) |
| `MacroSemanticStatus` (`template_only`/`mixed`) | the ledger vocabulary; `template_only` upgrades to `syntax_only` at the choke point because tree-sitter validation always runs on written files |
| Registry + version + `inputs_schema` + effects + authority gates | the **recipe contract** (below) |
| `macro_*` MCP surface (8 tools) | retires at parity with the rest of the kill list |

**The function store — the lighter successor (operator direction
2026-06-12: "store()/load() for lambdas").** Salvages the load-bearing two
ideas of the retired `narf-capability-library.md` (§2's session tier, §4's
recall-by-name with source staying out of model context) and drops the rest
— no decay scoring, no lifecycle states, no capability negotiation, no
prepare-time ceremony, no four-tier taxonomy. Two rungs:

- **Session rung — `store(key, fn)` / `load(key)` learn functions.** The
  existing KV accepts a function value: the host persists its SOURCE
  (`Function.prototype.toString`), and `load` revives it into a callable.
  Zero new vocabulary — the idiom agents already know, extended to code.
  The one real constraint is documented, not engineered: functions must be
  **self-contained** (captured closure variables do not survive source
  round-trip). Source crosses into the isolate on load but never into model
  context — NARF §4's context economics, kept.
- **Durable rung — a plain `recipes/` directory.** A recipe is a
  self-contained JS function file with a doc-comment contract (inputs,
  required namespaces, effects, authority gates, **and usage idioms** —
  what `MacroDefinition` got right, as prose, plus the revive-once/store
  pattern inline). The header is the recipe's own doc surface: the reviver
  necessarily reads it at the moment of use, so per-recipe guidance lands
  better-timed there than behind a system-memory signpost (sm signposts
  remain for cross-cutting knowledge like query grammar, which no single
  artifact owns). Git is the registry; review is the trust; the version is
  the commit. Promotion is mundane: a proven stored function is
  written to the file (by the cell through the choke point, or by the
  operator); recall is `file_read` + the same revive. No `recipes.*`
  bindings unless discovery measurably hurts in probes — list/describe
  economics can ride `glob` + doc-comments first.

  *Pilot validated (probe-recipe-1, 2026-06-12):* an agent given only "this
  repo carries recipes/" listed the directory, read
  `rust-rename-symbol.js`, judged the doc-comment contract "trustworthy on
  its own", revived the function expression via eval, executed it
  (cross-file rename, cargo check green) — 7 turns, zero errored cells, the
  cleanest probe of the series. Its retro explicitly rejected index files
  and a `recipes.list` binding as premature at this scale, confirming the
  deferral; the only note was underusing `store(key, fn)`/`load` for
  revival (re-embedded the source in eval twice) — discipline, not surface.

**Refactor atoms demote.** v2 §7 made "a canned atom" the external
interface; under the recipe tier that wrapper is redundant indirection: the
recipe already carries the typed contract, and `bro_exec` already carries
dispatch — an MCP-only consumer (or a workflow step) dispatches a harness
worker with a recipe reference. The promotion ladder for refactor work
flattens to **improvised cell → recipe**; an atom is minted only if some
external consumer genuinely earns a standing typed contract, never as the
default end state. (Direction set in discussion 2026-06-12; supersedes the
"canned atom" wording in v2 §7 and the third rung of cell-dsl §7's ladder
for the refactor domain — revise those when promoting past proposed.)

**Salvage list for bbox-macros:** the Java sidecar emission backend and the
probe→code-nav bindings are real; the planner, registry, expr DSL, and the
`macro_*` adapters dissolve. `unified-code-synthesis-model.md` is
superseded-in-part by this section. Same strangler discipline as
everything else: nothing retires until the recipe path holds under probes.

## 6.7 Two tiers: facts (surgical, bounded) vs analysis (reduce Rust-side)

The Java decomposition campaign (probe-dash-1/2 against a ~3,700-line
Vaadin view god class, 2026-06-12) forced a distinction §5 named but never sharpened:
**`code.*` facts and `analysis.*` are different tiers with different
payload contracts, and conflating them breaks at god-class scale.**

The failure that taught it: a god-class decomposition needs the *cohesion
structure* — which methods belong together. Driven through `code.*` facts
it fails two ways, both observed:

- **Sweep and die.** `code.query({files: <all ~1,700>})` to count where each
  field is referenced flattened to a multi-MB capture array that OOM'd the
  V8 isolate (~1.5 GB default heap; values-not-refs means big values live in
  the isolate — §2). Fixed with an aggregate cap (`MAX_AGGREGATE_QUERY_CAPTURES`),
  but the cap is a *guardrail*, not the answer: it truncates, and truncation
  is bounded-loss.
- **Avoid the sweep and grind.** The next probe stayed under the cap by doing
  ~50 targeted `code.query`/`code.read` cells, reconstructing the
  field-touch/call graph in JS by hand. It got the *right* answer (a clean
  7-method seam, avoided the forwarded-field trap, compiled green) — at
  52 cells, 50 of them analysis.

Same missing capability, two faces. The resolution is the tier split:

| | **facts (`code.*`)** | **analysis (`analysis.*`)** |
|---|---|---|
| Answers | "where is X?" (for editing) | "what is the structure of X?" (a question) |
| Returns | hash-anchored Spans | a small reduced structure |
| Reduction | none — raw nodes | **Rust-side**; raw intermediate never enters the isolate |
| Payload | aggregate-capped (never sweep a repo for raw spans) | bounded by construction (the answer is small) |
| Example | `code.query` captures | `analysis.cohesionClusters` → cluster graph |

Crucially the analysis tier is **still values-not-refs** (§2): the returned
value is the reduced answer, computed where the data already lives. The
platform principle was never "rebuild every Rust computation in JS from raw
facts" — it was "compose bounded values." Treating `code.query`
raw-capture composition as a substitute for a reduction binding was the
error. The cap stays as the facts-tier guardrail (its hint can point at
`analysis.*` once richer reductions land); the heap stays at V8 default
(raising it only defers the same OOM on a bigger repo — operator call,
2026-06-12: "give the agents better tools and prose, not a bigger heap").

**First analysis binding (shipped):** `analysis.cohesionClusters(file)` —
the v1 `extract_java_class_cohesive_clusters` planner ported as a binding
(thin adapter over `bbox_refactor::plan`, the `java.extractClass` pattern).
Returns the reduced cluster graph: per-cluster `{name_hint, item_names,
move_fields, score, expected_wiring}` + `cross_cluster_calls`, each cluster
ready to feed `java.extractClass`. One fix the port forced: the v1 walker
counts the **constructor** as a method, and a constructor assigns ~every
field, so its `method_to_field` edges transitively merge all concerns into
one cluster — fatal on the exact god classes this targets. Constructors are
never extracted to a delegate, so the clustering now drops them
(`cohesive_clusters.rs`); strictly correct for cohesion, benefits the
retiring MCP path too.

**Honest limitation (carried open).** On the real view god class the
binding returns 5 clusters but the dominant one is a 56-method megablob
(transitive field-sharing collapses concerns whenever a single *connector*
field — a shared container, a `getFreshData`-style dispatcher — bridges
them). It still beats 50 cells of grind (the agent gets scored, wired,
extract-ready seams including a clean 9-method `on*`-handler cluster), but
transitive closure is too coarse for deeply-tangled classes. The refinement
— connector-aware clustering (down-weight or cut high-fan-out bridge
fields/methods before partitioning), better `name_hint` inference — is a
real analysis-quality project, filed as a gap, not blocking the tier.

This is the third tenant shape after the refactor algebra and the transform
toolbox: a domain joins `analysis.*` by shipping one reduce-Rust-side
binding + a compact `analysis.describe` contract, zero runtime changes
(cell-dsl §8). Candidate siblings: `analysis.dependencyGraph`
(`java_class_dependency_analysis`), `analysis.references` (the count-mode
gap-31dc1375 wanted), `analysis.captures` (§5's extract-class support).

## 7. Validation: live probes + tailored retro

Patterns earn their place by probe, not by this doc:

1. Land v0 bindings behind `code_mode=only` dispatch on a fixture repo.
2. Dispatch bro-harness agents at the three programs' tasks *as tasks* (not
   as scripts) — the agent writes the cell against the declarations alone.
3. Retro each probe with an isolate/refactor-tailored variant of
   `prompts/RETRO_HARNESS.md` (to be authored alongside the first probe):
   binding discoverability under D3, declaration fidelity vs serde reality,
   batching ergonomics, bounce/findings actionability without re-discovery,
   apply-gating friction, KV continuity across cells.
4. File friction as gaps in the existing `*/refactor-tools/*` dedupe
   namespace; measure against the D5 baseline cluster.

## 8. Relationship

- **Executes** [`refactor-tools-v2.md`](./refactor-tools-v2.md) §5's spike;
  tenant of [`code-mode-cell-dsl.md`](./code-mode-cell-dsl.md).
- **Grounds against** code at `65420f2`; supersedes the v2/cell-dsl docs'
  runtime claims where §1 deltas say so (D1–D4).
- **Answers** gap-374fba64, gap-5d17610b, gap-de62454a, gap-903f6949 by
  dissolution (see §5); disposition of the gap notes themselves is an
  operator call at landing time.

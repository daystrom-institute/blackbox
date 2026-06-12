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

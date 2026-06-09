---
title: "Refactor tools v2: the in-box DSL"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - refactor-tools
brief: "Reformulate the refactor machinery as code-mode sandbox DSL: APIs placed in the V8 cell so the agent stitches refactors programmatically, replacing the v1 catalog of 100+ hand-written plan kinds. Diagnosis: v1 (~35K LOC bbox-refactor) is a programming language that predates having one — per-kind monolithic Rust functions, parameters that are really function calls (deep_analysis, wiring_mode), and bbox_refactor_run as a hand-rolled JSON workflow language with an interpreter; the slice/clip families are workarounds for not having variables. Re-cleave: v1's orchestration dissolves into cell programs; its analysis + edit algebra survive as the cell's standard library (facts: code.*/lsp.*/analysis.*; algebra: EditSet builder; one mutation choke point: apply()). Trust model designed for the post-adjudication-boundary reality (NARF retired, commit 9fcbd5f): no boundaries the model must respect, only invariants that hold regardless of what the cell does — binding shape, lineage-computed semantic_status on the artifact, mechanical validation with rollback, and adjudication inverted from pipeline stage to exception handler (apply bounces with structured findings; judgment is summoned, never scheduled). The v1 catalog inverts into a script library; the acceptance test for the binding algebra is that each v1 kind is expressible as a short program. Migration (§7) is a harness-first strangler: build bindings to used-kind parity behind the live daemon catalog (same substrate crates, no behavioral fork), then retire the MCP refactor surface entirely — refactor tooling becomes IN-HARNESS ONLY (decided); external/MCP-only agents (interactive operators, Claude-provider bros) direct refactoring via ad-hoc bro_exec/bro_resume orchestration or consume a canned atom. RX-V2 retires with the surface, before any daemon/worker split. Sibling of remote-worker-boundary.md; v2 of the design/refactor-tools/ intent."
---

# Refactor tools v2: the in-box DSL

> **Status: proposed.** Targets the codex-native code mode that actually
> shipped (`crates/bro-code-mode`; commit `9fcbd5f` "adopt Codex code-mode
> (exec/wait); retire NARF + cells"). The NARF docs in this folder are
> partially superseded; where this doc reuses their vocabulary (in-box/out-box)
> it is as shorthand, not as a dependency on the retired taxonomy — see §2 for
> what specifically did not survive and why this design does not rest on it.

## 0. Thesis

The v1 refactor tooling and the code-mode sandbox were built to solve the same
two problems: **stop using the agent's context as a copy/paste clipboard**, and
**mechanize common refactoring processes** as force multipliers for the
harnessed agent. v1 predates the sandbox, so it solved them the only way
available — a closed catalog of named transforms (`extract_java_class`,
`rewrite_rust_error_type`, …) behind MCP tools, plus a clipboard/slice surface
for moving bytes without context. That made `bbox-refactor` a **programming
language that predates having one**: 100+ plan kinds, each a monolithic Rust
function; parameters that are really function calls; and `bbox_refactor_run` —
ordered steps, rollback semantics, repair obligations, a command allowlist — a
hand-rolled JSON workflow language with an interpreter in the daemon.

Now there is a real composition runtime. The re-cleave:

- **v1's orchestration layer dissolves into cell programs.** A refactor is a
  short JS program the model writes (or recalls from a library) over a small
  algebra of bindings.
- **v1's analysis and edit-algebra layers survive as the cell's standard
  library** — the parts that are genuinely hard, genuinely Rust-shaped, and
  that the model must never be left to re-derive in JS: tree-sitter facts, LSP
  authority, capture analysis, the EditSet representation, and one guarded
  mutation choke point.
- **The clipboard objective is solved by the runtime itself**: a JS variable
  *is* a ref. Bytes flow read → transform → write without touching model
  context unless `text()`'d. The six-tool `bbox_slice_*` family and the
  `clip_*` registers stop being load-bearing.

The trust model is designed for the post-adjudication-boundary reality: **no
boundaries the model must respect, only invariants that hold regardless of what
the cell does.** The bindings are the policy, the artifact is the audit,
validation is the judge, and the model is the appellate court — involved when
something bounces, never as a scheduled checkpoint.

## 1. Diagnosis of v1 (grounded)

`crates/bbox-refactor` is ~35K LOC. The catalog (`plan_kinds()`,
`lib.rs:1279-1516`) enumerates 100+ kinds across Rust (~20 tree-sitter, 4
LSP-backed), Java (~60 including Vaadin/JooQ-specific), C# (mixed
LSP/sidecar), Elixir, plus generic text kinds. Dispatch (`lib.rs:1818-1982`)
maps each kind name to a hand-written `plan_X()` in a per-language module
(`rust.rs` ~6K LOC, `java.rs` ~8K LOC). There is **no combinator layer**: plans
compose by imperative construction inside each function, and the shared
substrate is thin — `FileEdit`/`TextEdit` byte spans + `original_sha256`
(bbox-corpus-core), the `semantic_status` enum, `ValidationStep`s, and the
capture-analysis structs (`CapturedVariable`, `ExternalCall`,
`InheritedDependency`, `lib.rs:167-299`).

Symptoms that the catalog was straining to be a language:

- **Parameters that are function calls.** `deep_analysis: true`,
  `rewrite_remaining_accessors`, `wiring_mode`, `recommended_resolution` — each
  is a branch in a program the caller cannot otherwise write.
- **Composites pretending to be kinds.** `extract_java_class` carries its own
  wiring strategies, FIXME taxonomy, and sub-operation sequencing — a program
  with a parameter schema for a CLI it never got.
- **`bbox_refactor_run` is an interpreter.** Ordered Plan/Command steps,
  per-step snapshots, rollback on failure, repair obligations, and RX-V2's
  command allowlist — control flow, error handling, and a security model for a
  JSON language nobody wanted to write in.
- **The slice family is variables, badly.** Six MCP tools
  (`bbox_slice_read/move/copy/delete/insert_text/replace`, `src/slices.rs`) to
  express what is one line of JS over two reads and an edit.

What v1 got **right**, and what this design preserves: the edit representation
with content hashes; atomic writes with rollback; `semantic_status` as honest
provenance (`lsp_verified` / `syntax_only` / `indexed_hints` /
`lsp_partially_verified`); fail-closed LSP (RX-V3); `plan_status: Blocked` and
FIXME-count gating as *detected-condition* refusal; and the capture-analysis
machinery that makes silent miscompiles loud.

## 2. The runtime this lands on — and the boundary that died

The shipped code mode is codex-native (`crates/bro-code-mode`): an `exec` tool
runs an async JS module in a V8 isolate; enabled tools project as a `tools.*`
object (`runtime/globals.rs:47-63`) whose calls trampoline through the
`ToolCapability` seam with the same deny filter as the flat surface
(`bro-harness/src/code_mode.rs`); `text()`/`image()` are the only egress into
model context; `store`/`load` persist values across cells; tool results
resolve in batches at quiescent boundaries; typing is TS declarations rendered
from JSON Schema into tool descriptions (`description.rs:442-705`). There are
currently **no refactor or code-nav bindings in-box**.

The NARF design this folder previously carried placed an **adjudication
boundary** through the runtime: interpretive results (search, selection) must
round-trip the model before anything depends on them; cells compose only over
exact, already-adjudicated inputs. That boundary **fell apart on first contact
with reality**, and the failure mechanism matters because it dictates this
doc's trust model:

1. **The economics point the wrong way.** Code mode exists to collapse model
   round-trips; a mandatory mid-pipeline round-trip taxes every happy path to
   guard the exceptional one. Ceremony with that cost profile gets skipped,
   rubber-stamped, then compacted away.
2. **Authorship already is adjudication.** When the model writes
   `hits.filter(h => h.path.endsWith("_test.rs"))[0]`, judgment was exercised
   at authoring time — query, predicate, strategy chosen with full knowledge of
   intent. Selection did not sneak into the box; judgment moved from
   review-time to policy-time. What is lost is seeing the *data* before acting
   — a loss that only matters where wrongness is silent.
3. **It was only ever prose.** The placement taxonomy admitted it could not be
   a runtime check ("a 'remember to inspect' rule dies at compaction" —
   narf-tool-placement §3). A boundary that lives in binding docs is already
   dead.

Refactoring is unusually suited to the surviving alternative because **its
wrongness can be made loud mechanically**: parse errors, compile failures,
test failures, public-API-guard diffs catch careless selection after the fact,
cheaply, with rollback. The genuinely silent failure modes — wrong-instance
delegate calls, stale captured values — are enumerable and *detectable by the
capture-analysis machinery v1 already built*. So: detection always-on at the
choke point, judgment summoned on detection, never scheduled (§4).

## 3. The DSL — three strata

Bindings placed in the cell, backed by crates that already exist and legally
link into the harness (`bbox-code-nav`, `bbox-lsp`/`bro-lsp`,
`bbox-corpus-core` edit types; the forbidden edge is only
`bro-harness → blackbox`).

### 3.1 Facts (read-only, exact)

```ts
code.parse(file): Tree                    // tree-sitter, pure function of bytes
code.query(file, tsQuery): Node[]         // spans, kinds, names
code.items(file): SyntaxItem[]            // the v1 refactor_status inventory
lsp.refs(file, pos): Location[]           // server authority; fail closed (RX-V3)
lsp.hover / lsp.symbols / lsp.diagnostics
lsp.rename(file, pos, newName): WorkspaceEdit
lsp.codeActions(span): CodeAction[]       // harvest server-shipped refactorings
lsp.resolve(action): WorkspaceEdit
analysis.captures(span): CapturedVariable[]      // the hard 20% — stays Rust
analysis.externalCalls(span): ExternalCall[]
analysis.inheritedDeps(span): InheritedDependency[]
```

`analysis.*` exists because the failure mode to design against is the model
re-deriving half-baked capture analysis in JS each session. Hard semantic
analysis is a *fact-returning binding*, never an exercise for the cell.

`lsp.codeActions` + `lsp.resolve` deserve emphasis: rust-analyzer and jdtls
already ship large assist inventories that v1 partially reimplemented as plan
kinds. Three bindings make the entire server inventory available — enumerate
(exact) → the model picks while authoring the cell (judgment at policy-time,
§2) → resolve and merge the `WorkspaceEdit` (exact) — and no per-assist plan
kind is ever written again.

### 3.2 The edit algebra (pure — builds, never writes)

```ts
const es = edits.begin();
es.replace(span, text);                   // span carries file + original_sha256
es.moveSpan(srcSpan, target, position);   //   captured at read time — drift
es.createFile(path, content);             //   guarding is structural, not remembered
es.merge(workspaceEdit);                  // LSP-authored edits join the same set
```

An `EditSet` accumulates `FileEdit`s (byte spans + replacement + source hash) —
the v1 representation, kept verbatim. Server-authored and cell-authored edits
compose into **one artifact type**, which is also the unit the integration
boundary of [`remote-worker-boundary.md`](./remote-worker-boundary.md) §4.2 can
accept when workers are remote.

### 3.3 The choke point (the only mutation)

```ts
const result = await apply(es, {
  validations: ["tree_sitter_no_errors", {compiler_check: "cargo check"}],
});
// result: Applied { diff_summary, semantic_status, validations }
//       | Bounced { findings: Finding[] }   // structured, repairable
```

`apply()` owns everything v1's apply/run layer got right: hash verification,
atomic writes, snapshot/rollback, validation steps, FIXME gating. Three design
rules, all consequences of §2:

- **Lineage-computed `semantic_status`.** The EditSet is stamped with the
  weakest provenance among the bindings that produced its edits: pure
  `lsp.rename` output → `lsp_verified`; mix in a `code.query`-derived span →
  `syntax_only`. The taxonomy survives as a computed property of the artifact
  instead of a per-kind declaration — and it matters *more* now, because
  nothing reviewed the pipeline mid-flight; the artifact is the only trust
  record.
- **Detection always-on; authority non-authorable.** v1's `deep_analysis` flag
  was a detection step the agent had to remember to request — the same
  dies-at-compaction failure as the adjudication boundary. In v2, capture/
  external-call/API-guard detection runs unconditionally at `apply()`.
  Operator-authority opt-outs (RX-V1: `acknowledge_repr`,
  `acknowledge_public_api_change`) arrive as **dispatch-time inputs** the cell
  cannot author; consumed opt-outs are recorded on the artifact
  (`operator_opt_outs_used`), preserving the audit invariant.
- **No unconditional confirm.** A `confirm: true` the cell can author in the
  same expression is theater. Clean EditSet on a recoverable worktree →
  applies; git is the net (hygiene, not safety). Gates fire only on *detected*
  conditions: Blocked findings, FIXME > 0, hash drift, validation failure.

## 4. Trust model: adjudication as exception handler

The single design principle: **build invariants that hold for adversarially
careless cell code.** Trust derives from exactly three places —

1. **Binding shape.** One mutation choke point; facts and algebra are pure;
   LSP fails closed; operator flags are non-authorable in-box (RX-V1 is the
   one governance pattern in this repo that survived contact with reality:
   authority shaped at the dispatch boundary, not behavioral rules).
2. **Artifact provenance.** Hashes, binding lineage, `semantic_status`,
   opt-outs-consumed — carried by the EditSet, audited at integration.
3. **Mechanical post-hoc validation** with rollback.

— and judgment enters through one door: **the bounce.** The cell runs
optimistically, search→select→transform→apply in one program (the design
target, not a smell). When detection finds a condition that genuinely needs
judgment, `apply()` refuses with structured findings. The model adjudicates
*then* — repairing programmatically in the next cell, escalating to the
operator, or supplying a dispatch-granted opt-out — on the exceptional path
only.

This makes the **findings vocabulary the real protocol surface** of v2. v1's
deep-analysis structs (`CapturedVariable` with mutability classification,
`ExternalCall` with `recommended_resolution`, `InheritedDependency`,
`FixmeCount`) are the seed vocabulary; they were wired to the wrong trigger.
The bar for a finding: a bounced cell can act on it *without re-running
discovery* — each finding carries the span, the classification, and the
candidate resolutions, so the repair cell is short.

Cheap **visibility replaces ceremony**: `notify()` streaming during long
cells, a diff summary in every `Applied` result, post-hoc trace. Observable
consequences, never mandatory review.

## 5. The catalog inverts into a library

The 100+ plan kinds are not ported; they become the **seed corpus and the
acceptance test**:

> **The design test for the binding algebra:** every v1 kind must be
> expressible as a short program over the bindings. Where the honest answer is
> "300 lines of JS reimplementing borrow classification," that is a missing
> `analysis.*` binding, not a longer script.

Proven programs become named, versioned **library scripts** — curated JS source
the model recalls instead of re-derives, restoring the repeatability that
hand-written Rust kinds had. Promotion path: improvised cell → named library
script → (where a contract is earned) an atom. "Mechanized common refactors"
stays a product feature; it stops being a Rust surface that grows by one
function per refactor per language. Reproducibility is honestly traded:
improvised one-off transforms are less reproducible than `kind:
extract_rust_items` was — accepted, recorded here as a decision, with the
library as the mitigation for everything common.

**First spike — the three-kind pressure test.** Hand-write the cell program
for three kinds spanning the difficulty range, before building anything:

1. `rust_lsp_rename` — trivial: one `lsp.rename` + `apply`. Validates the
   choke point and lineage stamping.
2. `extract_rust_trait` — mid: `code.items` + `analysis.captures` + algebra.
   Validates the facts/algebra split.
3. `extract_java_class` — worst case: capture analysis, wiring strategies,
   FIXME policy. Validates the findings vocabulary and the bounce loop; if
   this one needs a binding the other two didn't reveal, that binding was
   always going to be needed.

The minimal binding set falls out of the exercise; resist designing it on
paper first.

## 6. What retires, what remains

| Surface | Fate |
|---|---|
| `bbox_slice_*` (six tools) | retired with the MCP refactor surface (§7) — variables replace them in-cell |
| `clip_*` as refactor plumbing | same |
| `bbox_refactor_run` Command steps + RX-V2 allowlist | retired with the MCP surface (§7) — the allowlist was prompt-discipline compensating for no composition runtime (see sibling doc §3); `shell.run` + validations replace it |
| Per-kind plan functions + param schemas | dissolve into library scripts over the algebra |
| `bbox_refactor_plan/apply` flat MCP tools | **retired — refactor tooling becomes in-harness only (§7, decided).** External/MCP-only agents direct refactoring via `bro_exec`/`bro_resume` orchestration or consume a canned atom; no vestigial MCP projection |
| Edit representation, hash guards, rollback, validations | kept verbatim inside `apply()` |
| Capture analysis, tree-sitter queries, LSP session pool | kept as `analysis.*`/`code.*`/`lsp.*` bindings |
| `semantic_status` | kept, upgraded to lineage-computed artifact property |
| RX-V1 | kept, relocated: dispatch-time inputs + artifact audit field |
| RX-V3 | kept verbatim: `lsp.*` bindings fail closed, never downgrade |

## 7. Migration: harness-first strangler (in-harness only at the end — decided)

Build the harness-facing surface first, leave the daemon catalog untouched,
reach parity, then drop the MCP-side tooling. The split inherits one less
concern. Why this ordering is unusually cheap here:

- **Zero daemon changes to start.** The bindings link the same
  daemon-independent crates (`bbox-refactor` substrate, `bbox-code-nav`,
  `bbox-lsp`) that the daemon's thin adapters wrap — two surfaces over one
  engine during overlap, **no behavioral fork to keep in sync**.
- **The safety net stays live.** The daemon catalog keeps working while the
  bounce/findings/apply model is exercised by real agents; an insufficient
  binding mid-migration breaks nothing.

**Parity target: used-kind parity, not catalog parity.** Mine transcripts and
atom invocations for which kinds are actually called; classify the catalog
expressible-today / needs-a-binding / nobody-will-miss-it (a large slice of the
campaign-specific Java kinds is plausibly the third bucket). The §5 three-kind
pressure test generalizes into this sweep. "Beyond parity" arrives early via
`lsp.codeActions` — server assist inventories v1 never wrapped.

**The endpoint is decided: refactor tooling becomes in-harness only.** No
vestigial MCP projection. The consumer populations that only speak MCP —
interactive operator sessions, Claude-provider bros (Claude dispatches through
the Claude Code CLI, not bro-harness), external agents — get mechanized
refactoring by **directing a harness worker via ad-hoc `bro_exec`/`bro_resume`
orchestration, or by consuming a canned atom** whose implementation dispatches
the cell path. Consequence: the atom contract surface becomes the external
refactor interface, and the `sm-refactor-*` runbooks re-point at orchestration
recipes instead of tool sequences.

**Remaining gate: `bro-lsp` grows multi-language.** Parity for `lsp_verified`
kinds needs jdtls (and eventually roslyn) harness-side — the
`bro-lsp`/`bbox-lsp` fork already flagged in
[`backlog-diagnostics-truth-tiers.md`](./backlog-diagnostics-truth-tiers.md).
Overlap cost: two warm pools per host against the same projects, bounded by
idle eviction; tolerable, named.

**The staged drop** (ordering is most of the work):

1. Re-point the `sm-refactor-*` runbooks and refactor atoms at the cell /
   orchestration path — the runbooks are the interface agents actually follow,
   so this step *is* the migration.
2. Remove `bbox_refactor_*` / `bbox_slice_*` from default MCP surfaces and tool
   docs so new sessions stop learning them.
3. Delete the daemon adapters.
4. Salvage before deleting: per-kind *orchestration* code dies, but some kinds
   (notably `extract_java_class`) contain analysis worth extracting into
   `analysis.*` bindings first.

**Payoff at split time:** refactor tooling is already worker-local, so the
[`remote-worker-boundary.md`](./remote-worker-boundary.md) split never has to
move it — and RX-V2 needs no containerized successor or deprecation argument,
because the surface it governs is gone before the split happens. The migration
retires the invariant instead of relocating it.

## 8. Tensions, named honestly

- **Latency vs cell semantics.** Cold rust-analyzer is multi-second against a
  10s default `yield_time_ms` (`runtime/mod.rs:31-37`). Requires the warm
  session manager riding in the harness (growing `bro-lsp` from Rust-only
  toward `bbox-lsp`'s multi-language pool) and the pending/batching path for
  long LSP ops rather than sync awaits. Planning is the slow part; apply is
  fast — the "refactoring is incompatible with cells" intuition is wrong, but
  only if sessions are warm.
- **The typing surface becomes load-bearing.** Schema-rendered TS declarations
  work for flat tools; a DSL with `Span`/`Node`/`EditSet`/`Finding` value
  types flowing *between* bindings needs curated, hand-authored declarations in
  the exec description — cheap, but real authored surface, and the v2 home of
  what the `sm-refactor-*` runbooks did for v1.
- **Bind-mount rung keeps the guards.** Per
  [`remote-worker-boundary.md`](./remote-worker-boundary.md) §6, hash guards
  and drift checks stay load-bearing while workers share the host tree; only
  rung-3 isolation makes them pure hygiene.
- **Discoverability of the algebra.** A model that doesn't know
  `analysis.captures` exists will hand-roll it badly. Binding docs, the typed
  declarations, and library scripts that *demonstrate* the bindings are the
  mitigations; this is a prompt-surface problem and should be treated as one.

## 9. Relationship

- **Sibling of** [`remote-worker-boundary.md`](./remote-worker-boundary.md) —
  that doc owns topology/residency and proves the daemon needs no reach-in;
  this doc owns the worker-side execution surface that makes that true.
- **Tenant of** [`code-mode-cell-dsl.md`](./code-mode-cell-dsl.md) — the
  platform layer: value model (values, not refs), the hash-anchored Span,
  the host-side provenance ledger that makes §3.3's lineage-computed
  `semantic_status` mechanically honest, the namespace contract this doc's
  `code.*`/`lsp.*`/`analysis.*`/`edits.*` strata instantiate, and
  session/batching semantics. Cross-cutting infrastructure questions belong
  there, not here.
- **v2 of** the `design/refactor-tools/` intent corpus: supersedes the
  orchestration-layer intent of
  [`../refactor-tools/context-clipboard-refactor-primitives.md`](../refactor-tools/context-clipboard-refactor-primitives.md)
  and [`../refactor-tools/refactor-compound-runs.md`](../refactor-tools/refactor-compound-runs.md)
  (the clipboard objective is solved by the runtime; compound runs are
  programs); **preserves** the analysis/edit-substrate intent of
  [`../refactor-tools/ast-refactor-mechanization.md`](../refactor-tools/ast-refactor-mechanization.md)
  as the binding library. Verify against
  [`../refactor-tools/refactor-tools.md`](../refactor-tools/refactor-tools.md)
  when promoting this past proposed.
- **Builds on** the shipped codex-native code mode (commit `9fcbd5f`), not the
  retired NARF cell taxonomy; §2 records which NARF premise died and why the
  trust model here does not depend on it.
- **Carries** RX-V1/RX-V3 forward in relocated/verbatim form (§6); RX-V2
  retires with the shared-filesystem rungs.
- **Touches** [`bro-harness-tool-surface.md`](./bro-harness-tool-surface.md)
  (the flat tool surface this thins) and
  [`bro-harness-diagnostics.md`](./bro-harness-diagnostics.md) (diagnostics
  truth-tiers are the same lineage idea applied to compiler output).

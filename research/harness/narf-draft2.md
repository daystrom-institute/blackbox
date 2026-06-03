---
corpus: blackbox-research
kind: research-hub
track: harness
status: researching
confidence: mixed
generated_by: claude
last_reviewed: 2026-06-02
topics:
  - narf
  - metatools
  - atoms
  - bro-harness
  - authoring-layer
  - bounded-recursion
---

# NARF Draft 2 — the native authoring layer as the primary harness interface

This is the second pass at NARF. It is **not** a re-dump of the v1 braindump
([narf.md](narf.md)) — it is tighter, normative, and grounded in the actual
atoms/code-mode/bro-harness code rather than the felt shape of a session. v1
asked "what could we fuse?" This doc asks **"what should the canon be, and what
does it mean to make a programmable authoring layer the agent's *primary*
interface to the harness?"**

It is adjacent to, not a supersession of, v1: v1 keeps the breadcrumb map and the
exploratory script sketches; draft 2 holds the canon. It also feeds, and is fed
by, the [metatools axis](metatools.md) — NARF is the concrete answer to that
axis's open invariant *"is there a bro-harness path from ref-chaining to a
scriptable composition runtime?"* (`metatools.md`, Open invariants).

## 0. Thesis (the one-paragraph canon)

> The next-gen harness interface is **not a flat list of tools the model calls
> turn by turn**. It is a programmable, sandboxed authoring layer in which the
> agent writes a typed program that composes **capabilities** (atoms) over
> **refs** (values that never enter context), resolves **promises** (pending
> work, local or durable), and applies **plans** inside a **transaction** (edits
> that commit or roll back as a unit). The leaf of that composition — the atom —
> is polymorphic in backend: it may be three lines of deterministic Rust, a
> single bro, an ensemble of bros under a supervision loop, or an external
> adapter, and the author calls all of them the same way. Granularity becomes a
> property of the leaf, not of the runtime. And because a proven program can be
> distilled into a new atom, the authoring layer is **self-hosting**: it grows
> its own vocabulary by bounded recursion.

Everything below is the unpacking.

## 1. What grounding in code corrected from v1

v1 proposed several things as *new design targets* that the shipped code already
provides. The canon must build on what exists, not re-invent it.

1. **The two-tier promise model already exists as `AtomImplementation`.** v1 §4
   said "NARF likely needs two layers — local promises + durable handles."
   `AtomImplementation` (`../../src/orchestration/atoms/types.rs:136`) already
   splits **synchronous** backends (`Deterministic{runner}`, `Adapter{...}` —
   `AtomRunner::run -> RunnerResult`, `../../src/orchestration/atoms/runners.rs:48`,
   zero dispatch cost) from **asynchronous, owner-resumed** backends
   (`Profile{brofile_ref}`, `Workflow{workflow_ref}` — returning an `AtomHandle`,
   `../../src/orchestration/atoms/invocation.rs:35`). NARF's `Promise<T>` should
   *unify over* this split, not parallel it.

2. **`Ref<T>` is an opaque host-side handle, not a JS-heap value.** Code-mode
   ships two ref substrates and does not reconcile them: `store`/`load` (JSON
   round-tripped through the V8 heap each cell —
   `../../../codex/codex-rs/code-mode/src/runtime/mod.rs:233`) and bro-harness
   clipboard registers (values that never enter V8 —
   `../../crates/bro-tools/src/clipboard.rs`). Canon: **big values live host-side
   as opaque handles** (`ref:slice/…`), passed through JS as string tokens,
   materialized only at the apply boundary. `store`/`load` is reserved for small
   control values. This resolves v1 open question #1.

3. **The composition / ownership / budget tree is already built and hardened.**
   v1 listed "transaction ownership when a script invokes an atom" as open.
   `validate_atom_invoke_policy` (`../../src/tools/atoms/composition.rs:9`)
   already enforces caller-owns-parent, `MayInvokeAtoms::{None,Any,Allowed}`
   composition policy, and a full **ancestor-chain walk** of per-ancestor
   `max_depth` + `dispatches_runs` budgets (`composition.rs:62-88`), with a
   64-deep cycle guard (`composition.rs:220`) and cost propagated up the chain
   (`composition.rs:250`). What is genuinely open is *edit-effect/rollback*
   composition — see §4 `Tx` and §8.

4. **Atoms are polymorphic-in-backend — this is the undersold thing.** v1 treated
   atoms as "callable refactor capabilities." The manifest
   (`AtomManifest`, `types.rs:30`) and the four-way `AtomImplementation` say an
   atom is the **universal unit of agentic work**: mechanical/deterministic,
   single-agent, multi-agent-ensemble, or external-service, behind one typed
   contract. §2 is the consequence.

## 2. The canonical reframe: the leaf is the innovation

The [metatools axis](metatools.md) frames a binary divergence: Codex code-mode
composes **raw tools** (fine grain); Claude Code Workflows composes **subagents**
(coarse grain). Its open invariant asks whether that split is a fundamental
tradeoff or a maturity gradient.

**Canon: it is neither — it is a false dichotomy.** Granularity should be a
property of the *leaf*, declared in the capability manifest, not a property of
the *runtime*. A NARF call site —

```js
const out = await atoms.invoke("refactor.rust.moveItem", input);
```

— is grain-agnostic. The backend decides whether that resolves as a 2 ms
`Deterministic` runner, a single `Profile` bro, or a `Workflow` that fans nine
bros through an oracle/advisor supervision loop. The author neither knows nor
cares. **The grain is hidden behind the contract and chosen by the runtime.**

Three consequences make this strictly more expressive than either reference
harness:

- **A continuum, not two points.** Atoms span `deterministic (zero-LLM) ↔ single
  bro ↔ ensemble`. A program over mostly-deterministic atoms is fast and nearly
  free, escalating to bro/ensemble atoms *only at the steps that need judgment*.
  This is an efficiency frontier a pure subagent-leaf runtime (Claude Workflows,
  this harness) cannot reach — every leaf there is a full inference. Atoms let
  the author spend inference surgically.

- **Reliability is declared once, in the manifest.** `AtomSupervisionPolicy` /
  `SupervisionPlan` (`types.rs:148`, `:167`) bake classifier → advisor → recovery
  tier-ladder into the capability. Calling a supervised atom buys ensemble-grade
  reliability *for free*. Contrast a subagent-leaf runtime, where adversarial
  verification is hand-authored as a `parallel([...refuters])` lattice **every
  composition**. The difference between "I write a verify loop every time" and
  "the capability supervises itself" is the difference between a composition
  *tool* and a composition *framework*.

- **bro-harness leapfrogs rather than catches up.** Its metatools row is empty
  today (`metatools.md`, convergence table). A runtime whose leaf is the atom does
  not merely match Codex/Claude — it occupies a strictly-richer point, because its
  leaf is the *union* of (raw tool ∪ subagent ∪ ensemble ∪ external adapter)
  behind a typed, versioned, effect-declared, supervised contract. See the
  leaf-grain dimension added to [metatools.md](metatools.md).

## 3. Bounded recursion — the fractality canon

The deepest property is that **the composition layer and the capability layer are
the same substrate at different maturities.** A program composes atoms. A *proven*
program is distilled into a new atom. That atom is then a leaf in future programs.
The framework grows its own vocabulary.

This is not aspirational — the promotion anchor already exists in the type system:
`AtomProvenance::Distilled { distilled_by, evidence_session_ids,
created_from_threads, accept_count, reject_count }` (`types.rs:850`). The atom
system already models capabilities **minted from prior sessions/threads with
accept/reject telemetry**. Script → atom promotion is therefore a new
*distillation source* (`distilled_by: "narf-script"`), not a new mechanism.

The canon word is **bounded**, not infinite. The recursion is governed, not
open: `MayInvokeAtoms` gates *who may call whom*, and the ancestor-chain
`max_depth` / `dispatches_runs` walk (`composition.rs:62-88`) caps *how deep and
how expensive* the tree may get. Fractal composition with a hard floor. (Historical
note: atoms — and this bounded-recursion shape — predate the Claude Workflows
public surface by weeks; the fractality was the obvious move once capabilities had
contracts.)

The self-hosting loop, stated as canon:

```text
author a program over existing atoms
  → it works, accepted N times (Distilled.accept_count)
    → freeze its inputs/outputs/effects into a manifest
      → register as atom:foo@v1
        → it is now a leaf in the next author's program
```

No row in the metatools table has this. Codex scripts are ephemeral (the isolate
dies, the vars die). Claude saved-workflows are persistent but *flat* — a saved
workflow is not a typed capability with an output schema, an effect declaration, a
cost class, a supervision policy, and a version. The **atom envelope** is what
turns "a composition I wrote" into "a contract the system can budget, supervise,
version, and compose."

## 4. The canonical primitives (tight)

Each primitive gets one normative sentence and its existing code anchor. This is
the vocabulary the authoring layer exposes.

- **`Ref<T>`** — a typed, settled value held host-side; the program passes an
  opaque handle, never the bytes. Generalizes the clipboard register
  (`bro-tools/src/clipboard.rs`); `clip:` stays as the user-facing alias.
  Materialized into context only via explicit, bounded egress (`text()` /
  `clip_peek`).

- **`Promise<T>`** — pending work that resolves to a `Ref<T>`, an error, or a
  cancellation. Unifies harness-local same-dispatch promises
  (`bro-tools/src/promise.rs`) with durable, owner-resumed atom handles
  (`AtomHandle`, `invocation.rs:35`). A resolved promise *deposits into a ref*
  (the existing `shell_run(mode=promise, stdout_to=…)` shape, generalized). Must
  also gain **`pipeline`** (no-barrier per-item staging) alongside the existing
  `when_all`/`when_any` barriers (`promise.rs:454`, `:485`) — stolen from this
  harness's Workflow runtime.

- **`Plan<E>`** — a typed proposed effect (usually an edit) with previews, parse
  validation, file hashes, and apply preconditions. Already real:
  `bbox_refactor_plan` / slice-derived plans (`../../src/refactor/mod.rs`,
  `../../src/slices.rs`).

- **`Tx`** — a transaction scope for applying plans/commands with rollback,
  obligations, and final validation (`bbox_refactor_run`'s rollback machinery,
  generalized). **Lifetime is bounded by the program cell**: a `Tx` that reaches
  cell-end uncommitted auto-rolls-back (RAII keyed to `cell_closed`,
  `../../../codex/codex-rs/code-mode/src/service.rs:100`). The unresolved canon
  question is **transaction vs saga** for nested atoms — see §8.

- **`Atom<I,O>`** — a named, versioned capability contract: input/output JSON
  schema, declared effects, composition policy, supervision policy, cost class,
  runtime intent, trace, provenance, and a backend binding
  (`AtomManifest`, `types.rs:30`; `AtomRef` = `atom:name@vN|@latest`,
  `types.rs:899`). Output is schema-validated by construction
  (`OutputShapeStatus`, `invocation.rs:72`); effects are declared *and observed*
  (`EffectsObserved.violations`, `invocation.rs:57`).

- **`Script`** — a bounded JS composition cell with access to the typed host
  bindings, no ambient fs/network/console (code-mode deletes
  `console/Atomics/SharedArrayBuffer/WebAssembly`,
  `../../../codex/codex-rs/code-mode/src/runtime/globals.rs:13`). The script is
  the unit of agent action.

## 5. What it means to make this the *primary* interface

This is the part the title promises and v1 only gestured at. The radical mode is
Codex's `code_mode_only` taken as the default, not an experiment: the
**model-facing** tool surface collapses to a tiny pair (`narf_exec` / `narf_wait`,
mirroring `exec`/`wait`, `code-mode/src/lib.rs:45`), and *all* capability —
atoms, code discovery, refactor, refs, promises, tx — lives as **bindings inside
the sandbox**, not as direct tools.

> **Decision (2026-06-02): NARF is a config-selected mode, not NARF-only.** The
> radical `code_mode_only` surface is the primary interface *for
> composition-capable models doing multi-step work* — but the harness preserves a
> **conventional flat-tool surface as a config option** for lower-tier models
> (classifiers, supervision advisors, cheap leaf workers) that can't author a
> composition cell and shouldn't be forced to. Both modes share everything below
> the model-projection layer (admission/surface filter, in-process `Tool::call`
> bindings, capability traits); only *how tools are presented to the model* — flat
> tool-call schemas vs JS bindings + `narf_exec` — branches. It is a tier
> spectrum, selected by the brofile/session config that already drives surface
> evaluation: structured-output-only (classifier) → flat tools → NARF composition.
> See [../../design/bro-harness/harness-daemon-boundary.md](../../design/bro-harness/harness-daemon-boundary.md) §9.

What actually changes when the authoring layer is primary, not optional:

1. **The unit of agent action becomes a verifiable program, not an opaque
   side-effect sequence.** Today a refactor is N model turns, reconstructable only
   from the transcript. As a program it is one turn that emits typed composition.
   That single shift cascades:
   - **Provenance**: `bbox_blame` points at *the program that authored this line*,
     not a turn. To make replay sound, record tool *results* alongside the source
     (a VCR cassette) — which is exactly this harness's journal model
     (`resumeFromRunId`, cached completed calls). Steal it.
   - **Review**: read 30 lines of typed composition, not a 40-message transcript.
   - **Resumption**: re-run the program; the journal replays settled calls.

2. **The agent stops being a turn-taking REPL and becomes a program author.** The
   three failure modes the metatools axis names — context pollution, latency,
   non-determinism (`metatools.md`, "The dimension") — are *collapsed by
   construction*, not mitigated per-turn. Intermediate refs never enter context;
   a whole refactor is one inference; the orchestration plan is fixed code, not
   re-derived each turn.

3. **The impedance mismatch closes.** Today the model reasons in prose/tokens, the
   system executes in typed effects, and tool calls are the lossy serialization
   between them. With refs/plans/tx/atoms as first-class bindings, **the agent
   thinks in the same types the system executes.** Refactoring is only the
   beachhead because that is where the mismatch hurts most (byte coordinates,
   diagnostics, rollback); the same runtime generalizes to migrations, audits,
   codemods, and `bro_*` orchestration itself.

4. **Discovery becomes input, not prose.** A primary authoring layer makes typed
   code discovery (`bbox_code_symbols` / `code_node_describe` / `code_refs`) feed
   *directly* into plans — `code.symbols().pick(...)` → `refactor.plan({source})`
   — so byte coordinates never round-trip through the model. This is the line
   between an agent that *describes* edits and one that *computes* them, and it is
   the differentiator a shell-only code-mode (Codex) structurally cannot reach
   without blackbox's typed graph.

The bet: a primary authoring layer is worth its weight only if the *common* case
is also expressible — a one-line read, a single edit — without ceremony. Canon:
trivial actions stay trivial (`await fs.read(path)` is a legal one-liner program);
the layer earns its keep on the multi-step work, and never taxes the single step.

## 6. The soft-dep seam (canon boundary)

> **Refined by** [../../design/bro-harness/harness-daemon-boundary.md](../../design/bro-harness/harness-daemon-boundary.md)
> §6/§9: when the harness runs *in-process in the daemon*, the corpus-capability
> seam is an in-memory `bro-capabilities` **trait**, not an MCP client. The MCP
> path below remains the standalone/degraded mode and the external-caller surface.

v1 §8 relaxed the no-daemon-runtime-dependency invariant for exploration but left
the seam vague. Canon, made precise by the code:

- **Harness-local substrate is daemon-free.** V8 runtime, refs, promises, shell,
  workspace, and a *local-file* `Tx` are workspace-crate code both processes can
  link. bro-harness runs alone, exactly as the existing invariant requires.

- **Capability leaves arrive over the existing MCP injection seam.** Daemon-backed
  bindings — atoms (daemon-resident: everything in `composition.rs` lives on
  `BlackboxServer`), the code graph, the refactor backend, durable coordination —
  are simply additional entries in the `tools` object, injected via
  `../../crates/bro-harness/src/mcp.rs`, exactly as Codex folds MCP tools beside
  native ones (`tools.mcp__blackbox__…`). The "soft dependency" is literally
  *"some `tools.*` entries are MCP round-trips."*

- **Absent daemon → the binding is absent → fail closed.** Semantic operations
  that need the daemon/LSP do not silently downgrade; they are simply not present,
  consistent with the repo's `RX-V3` fail-closed rule for LSP-backed plan kinds.

This rides the seam that already exists; it adds no new coupling and preserves
"harness runs without daemon" as a hard property.

## 7. Governance canon (non-negotiables)

The authoring layer is powerful for *composition*, never a host escape hatch.
These carry forward from the shipped systems and are not up for trade:

- Operator-authority flags (`acknowledge_repr`, `acknowledge_public_api_change`)
  are **never inferred** by the program — pass-through from operator input only
  (repo invariant `RX-V1`).
- LSP-backed semantic refactors **fail closed** when the LSP is unavailable
  (`RX-V3`); the sandbox does not get to approximate them.
- Mutating operations **outside a `Tx` refuse** (or require explicit operator
  confirmation); `Tx`-bounded mutation is the default, and effect-declaring atoms
  let the runtime refuse a write-effecting atom outside a `Tx` statically
  (`AtomEffects`, `types.rs:102`).
- Recursion is **bounded** — composition policy + ancestor depth/budget
  (`composition.rs`).
- The multi-agent **shared-worktree invariant holds at the apply boundary**:
  stale-hash + dirty-worktree protection from the refactor runner is inherited,
  never bypassed by the JS layer. (The sandbox makes composition *feel* local;
  this is where that illusion is most dangerous.)
- Egress is **explicit and bounded** — refs never auto-render; large tool *errors*
  are truncated/ref-wrapped; a script-level egress budget caps incidental
  `text()`.
- The JS runtime exposes **explicit globals only**, no ambient fs/network/console
  (`runtime/globals.rs:13`).

## 8. Open forks (decisions deferred to the next pass)

Stated, not resolved — these are the choices that change *which runtime* gets
built.

- **`Tx` vs saga for nested atoms.** `tx.absorb(atom.effects)` quietly assumes a
  real transaction (the atom runs in the caller's worktree, reports `touches`, and
  unwinds *with* the outer `Tx`). But the `Profile`/`Workflow` dispatch model
  delivers a *commit-point* (the atom is its own transaction; the outer scope can
  only sequence and compensate forward). The dispatch/ownership/budget tree is
  built; the **edit-rollback composition is not**. Pick: shared-worktree
  transaction, or compensate-forward saga.

- **Where V8 lives.** Inside bro-harness (keeps "harness runs alone," pays the
  `v8` + `deno_core_icudata` binary/build cost and the thread-per-isolate ↔
  `async ToolCx` bridge, `runtime/mod.rs:204`) vs a daemon-side runtime the
  harness talks to (cheaper harness, bends the no-daemon-dep invariant harder than
  §6's seam allows).

- **Ref store backend.** Harness-session-local, daemon-backed, or a shared
  abstraction with both — and the GC/persistence rules for refs, traces,
  diagnostics, and plans.

- **Egress accounting.** Per-script token budget on top of the clipboard's
  per-item bounds + LRU, and whether the budget is enforced by the runtime or
  declared by the program.

## Crosslinks / breadcrumbs

- v1 braindump (breadcrumb map + exploratory script sketches):
  [narf.md](narf.md).
- The axis this answers: [metatools.md](metatools.md) — see the leaf-grain
  dimension and the dissolved fine/coarse open invariant.
- Track charter: [harness-tracks.md](harness-tracks.md).
- Atoms canon in code: `../../src/orchestration/atoms/types.rs`,
  `../../src/orchestration/atoms/invocation.rs`,
  `../../src/orchestration/atoms/runners.rs`,
  `../../src/tools/atoms/composition.rs`.
- Codex code-mode (adaptable substrate):
  `../../../codex/codex-rs/code-mode/src/{lib,service,description}.rs`,
  `../../../codex/codex-rs/code-mode/src/runtime/{mod,globals}.rs`.
- bro-harness substrate: `../../crates/bro-tools/src/{tool,promise,clipboard}.rs`,
  `../../crates/bro-harness/src/mcp.rs`,
  `../../crates/bro-tools/src/fleet_worktree.rs`.
- Design corpus to feed: `../../design/bro-harness/bro-harness-clipboard.md`,
  `../../design/bro-harness/bro-harness-tool-chaining.md`,
  `../../design/refactor-tools/context-clipboard-refactor-primitives.md`.

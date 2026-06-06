---
title: "NARF typed cells: one authorial surface, the verb sets the tier"
kind: design
lifecycle: superseded
superseded_by: "codexification.md — bro-harness adopted Codex code-mode (exec/wait); NARF retired"
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - authoring-layer
  - orchestration
  - atoms
  - workflows
  - durable-execution
  - box-edge
brief: "The atom and workflow systems predate the V8 sandbox. This doc argues they collapse into ONE authorial surface — a cell: an author-supplied JS snippet plus a declared contract. The model-facing VERB, not a separate runtime, sets the persistence tier: narf_exec = ephemeral (runs now, returns a value), narf_register = a named reusable cell (the atom slot — register out-box, invoke in-box by ref), narf_registerWorkflow/narf_scheduleWorkflow = a durable cell whose long-lived waits are explicit daemon-owned handles, not implicit serialization of every JS await. Grounded in code: prepare today is JS-syntax-only plus declared contract validation (no TS inference — see bro-script/src/lib.rs prepare_one / validate_script_syntax), so 'typed' means a DECLARED contract block validated alongside the snippet, NOT an inferred TS signature. Ordinary awaits are raw-V8/Codex-code-mode-shaped live async host calls: a callback stores a PromiseResolver, Rust resolves it back into the same activation, and the daemon may yield/poll independently. The durable tier may reuse the existing IOCP-shaped park/resolve seam (WaitStore + signal_arc_dispatch + the arc loop's catch-up rescan), but only through explicit durable handles. Two daemon services survive the collapse: a durable wait/resume executor and a contract/catalog registry. The atom 4-backend taxonomy (profile/workflow/deterministic/adapter) stops being distinct runtimes and becomes 'what the cell body happens to do.'"
---

# NARF typed cells: one authorial surface, the verb sets the tier

> **Status.** Proposed; converged from a live design jam, then grounded against
> code on `beta/blackbox-v2`. This is a *synthesis* sibling, not a supersession:
> it sits on top of [`narf-data-model.md`](./narf-data-model.md) (the one durable
> KV), [`narf-capability-library.md`](./narf-capability-library.md) (the
> cell-local → session-local → library → atom ladder and the prepare/run split),
> and [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) (topology and
> the `bro-capabilities` traits). It does not retire the atom or workflow design
> docs yet — it argues for *where they go* once the sandbox is the substrate.
> Treat §1–§6 as the target; §7 is the honest gap list against current code,
> verified by file:line.

## 0. Thesis

> The atom system and the workflow engine were both authored **before** the V8
> in-process sandbox existed. Each invented its own authorial surface — atoms a
> JSON capability contract with four pluggable backends; workflows a JSON node
> graph with typed `next` edges and `wait` nodes. The sandbox now offers a third
> surface that is strictly more expressive than either: an author writes *code*.
> The claim of this doc: **there is one thing — a cell — and the verb you reach
> for sets its persistence tier.** You do not pick a runtime; you pick how long
> the thing lives.

Three tiers, one body:

| Verb | Tier | Lives | Backed by (today) |
|---|---|---|---|
| `narf_exec` | **ephemeral** | one call, returns a value | `run_one` (bro-script) |
| `narf_register` | **named/reusable** (replaces the atom; see §0.1) | cell-registry entry; invoke by ref | cell registry (storage MAY reuse the artifact catalog) |
| `narf_registerWorkflow` / `narf_scheduleWorkflow` | **durable** (the workflow slot) | an engine arc that parks and resumes | `WaitStore` + `signal_arc_dispatch` + arc loop |

The body is the same in all three: a JS snippet against the injected NARF
capabilities (`fs`/`search`/`git`/`shell`/`web`/`narf.kv`/`mcp.<server>.<tool>`/
`narf.encode`, per the data-model and tool-placement docs). What changes is the
*verb*, and the verb is a box-edge control: **you register/schedule out-box, you
invoke in-box.** (§3.)

### 0.1 What this collapse is NOT — read before implementing

This doc is misread in exactly one way, and the misread is expensive. **The cell
is the primary abstraction; the atom 4-backend taxonomy DISSOLVES into it.**

- ✅ **RIGHT:** `narf_register` writes a **cell** (source + contract) to a cell
  registry; the unit stored and invoked is a cell. The four atom backends
  (profile / workflow / deterministic / adapter) stop existing as distinct
  runtimes. `atom:<name>` survives ONLY as an invocation **handle** that resolves
  to a cell — see [`narf-data-model.md`](./narf-data-model.md): "`atom:` survives
  only as a handle, never as data composition." Storage MAY reuse the
  `bbox_artifact_*` catalog as a backing store, but that is a *storage* detail —
  the record in it is a cell.
- ❌ **WRONG — do not build this:** adding a fifth atom backend
  (`AtomBackend::Cell`, "cell-type backing") to the existing atom system so atoms
  can be *backed by* cells. That entrenches the 4-backend taxonomy this design
  exists to delete and inverts the hierarchy (atom-on-top, cell-underneath).

When the prose below says "the backend is a NARF cell" or "the atom slot," it
means the cell **replaces** the backends — never that a cell becomes one of them.
**If you are extending the atom backend rather than collapsing it into the cell,
stop: you are implementing the inverse of this design.**

## 1. What "typed cell" actually means — grounded, not aspirational

The jam used the word "typed" loosely ("the return type of the snippet is the
type of the cell"). Grounding in code forces precision, because **there is no
type machinery at all today.**

The entire `prepare` pipeline:

- `prepare_one` (`crates/bro-script/src/lib.rs:1368`) calls `render_prepare`
  (`:296`), which is **string concatenation** of resolved session-helper imports
  followed by the author's source. No parse beyond that.
- It then calls `validate_script_syntax` (`:1459`), which wraps the body in
  `(async () => { … })` and calls `v8::Script::compile` inside a `tc_scope`. This
  is a **JS syntax check only**: no execution, no `.d.ts`, no signature parse, no
  schema extraction.
- `PrepareResponse` (`:142`) is `{ ref_handle, status, diagnostics, source,
  contract }`. The contract field is declared input, not inferred from JS.

So the operator's read is correct about TS: *there is no TS wiring, and no
inferred type capture.* That has a hard consequence for this design:

> **"Typed" must mean a DECLARED contract, not an INFERRED TS signature.** There
> is no TypeScript toolchain in the daemon or harness. Building a real
> TS-signature → JSON-schema extractor is a new, non-trivial dependency
> (a TS compiler API or `tsc --emitDeclarationOnly` round-trip) and MUST NOT be
> assumed to exist. v1 of the typed cell declares its contract explicitly,
> alongside the JS body.

### 1.1 The contract block (v1)

A registered/durable cell supplies, next to its `source`, a declared contract:

```jsonc
{
  "source": "async function run(input) { /* … */ return { merged, count }; }",
  "contract": {
    "entry": "run",                         // the exported function the verb invokes
    "input":  { "type": "object", "properties": { "repo": {"type":"string"} },
                "required": ["repo"] },
    "output": { "type": "object", "properties": { "merged": {"type":"boolean"},
                "count": {"type":"integer"} } },
    "effects": ["shell", "git"],            // declared, NOT inferred (see §1.2)
    "may_invoke": ["atom:reviewer@v1"],     // composition allow-list
    "dispatch_budget": { "max_bros": 3, "max_depth": 2 }
  }
}
```

This is exactly the atom contract from `docs/atoms.md` (stable name, input/output
schema, effect upper-bounds, composition rules, dispatch/depth budget), but the
contract is satisfied by **a cell** — which *replaces* the four bespoke atom
backends, rather than adding a fifth (see §0.1). The contract
is validated two ways:

1. **At prepare/register time:** JSON-schema well-formedness + the `entry`
   function exists in the (syntax-validated) source. This is a small, additive
   extension to `prepare_one` — it already has the source and a V8 scope.
2. **At invoke time:** input is schema-checked before the body runs; output is
   schema-checked before it returns. A mismatch fails closed with a diagnostic,
   not a silent pass.

`PrepareResponse` gains an optional `contract` echo so the model reviews exactly
what it registered (the §0.1 review step, same spirit as `source`).

### 1.2 Effects are declared, never inferred

A cell that shells out, dispatches a bro, or writes the KV must *say so* in
`contract.effects`. The runtime does not sniff the body for `shell(...)` calls.
Two reasons: (a) inference is unsound under indirection (a helper import, a
`mcp.*` proxy call); (b) the effect set is the upper bound the
`ToolFilter`/recursion-guard enforces — it is a *capability grant*, and grants
are author/operator authority, not a guess. This mirrors `RX-V1` (operator-
authority opt-outs are never agent-inferred) and the box-edge rule that the box
does not widen its own surface.

## 2. The verb ladder maps onto tiers that already exist

The capability-library doc (§ the persistence-tiers table) already describes the
ladder `cell-local → session-local → NARF-lib → atom`. This doc adds the
*model-facing verb* for each durable step and shows the verb's tier is a thing
the daemon can already back:

- **`narf_exec` (ephemeral).** Runs `run_one` now, returns the value (bounded by
  the existing oversized-tool-result rider per data-model §5). Nothing persists
  beyond the session KV the body chose to write. This is unchanged.

- **`narf_register` (named/reusable — the atom slot).** Renders + syntax-checks
  the body (today's `prepare`), validates the contract (§1.1), and stores it as a
  catalog artifact (`bbox_artifact_*` kinds already include `atom`). Invocation is
  by ref: in-box `atom:<name>@<v>` deref (exact handle, the box-edge-legal kind of
  "ref" that survives in the data-model doc) and out-box discovery via
  `atom_search`/`atom_describe`. The 2-step authoring (`prepare` returns source
  for review → `register` commits) is the existing `narf_prepare`→`narf_run`
  pattern with a persist step bolted on.

- **`narf_registerWorkflow` / `narf_scheduleWorkflow` (durable — the workflow
  slot).** Same authoring surface; the body is allowed to *park* on external
  completion (a child bro, a webhook, a timer). Parking is not a JS `await` the
  cell holds across a restart — it compiles to an engine **wait** (§4). The
  scheduled variant is cell-native: a schedule wakes an exact cell handle and
  passes tick context into JS. The cell body is the evaluator; scheduling does
  not add routing packets, packet evaluators, workflow hook ops, or atom
  backends.

## 3. Box-edge placement of the verbs

Per the invariant ("the box never selects"; enumeration is the front half of
selection → out-box):

- **Out-box (model-facing):** `narf_register`, `narf_registerWorkflow`,
  `narf_scheduleWorkflow`, and all *discovery* (`atom_search`, listing registered
  cells, inspecting a contract). Authoring, naming, and launching a durable thing
  are model judgement calls — they belong to the awake model.
- **In-box (exact deref):** *invoking* an already-named cell by exact ref —
  `atom:reviewer@v1(input)` — and **awaiting host promises the live cell
  already holds**. In the ordinary case, `await shell.run(...)` / `await
  atoms.invoke(...)` is a Codex-code-mode-shaped live async boundary: the host
  call returns a JS promise, Rust performs the work outside the isolate, and the
  result resolves back into the same live activation. This is not, by itself, a
  model/daemon turn boundary and it is not restart-safe continuation
  serialization. Cross-turn or cross-restart durability requires an explicit
  daemon-owned durable handle (§4.0). The box may call/await what it was handed;
  it may not browse the catalog to decide what to call.

This is the same split the data-model doc draws for KV (`narf.kv.get(known-name)`
in-box; `narf_kv_list` out-box) and tool-placement draws for `mcp.*`.

## 4. The durable tier rides the existing park/resolve seam (it's your IOCP picture, already built)

### 4.0 Live async first; durability is explicit

The model underneath the sandbox is **not** "one wake on every await." The
baseline is Codex code-mode's live async cell:

1. JS calls a host binding.
2. The raw-V8 callback creates a `PromiseResolver`, records it by host-call id,
   emits a host-call event, and returns the JS promise immediately.
3. Rust executes the host work outside the isolate.
4. The runtime resolves/rejects the recorded promise and performs a microtask
   checkpoint, so JS continues in the same live activation.

The daemon/controller may yield output to the model, pause at a pending frontier,
or terminate the isolate, but ordinary `await` is a live-cell boundary, not a
serialized continuation. This avoids a turn per await and keeps normal cell code
JS-idiomatic.

Durability is a separate daemon-owned layer. A producer that intentionally
outlives the live cell (long shell job, child bro, timer, webhook, external
signal) returns an explicit durable handle. That handle can later be waited on,
joined, inspected, or used to start a new activation, but the runtime does not
pretend that V8 continuations survive isolate teardown or daemon restart. The
durable handle is the wait token; a plain JS promise is ephemeral unless a host
producer explicitly backs it with durable state.

The prior "wake-native" phrasing was an exploratory shortcut and must not be
implemented as a universal runtime axiom. The durable tier should lift only the
parts that are true: daemon-owned correlation, persisted completion records, and
explicit resume policy.

The operator described the durable mechanism as "vaguely TPL IO-completion-port
shaped: first cell awaits → under the covers an IRP/DPC → later the result comes
back and the loop re-awakens and resumes from the callsite." That is not an
analogy to build toward — **it is the literal implementation of workflow wait
nodes today.** Grounded:

- **The await.** `run_wait_node` (`src/workflow/engine/wait_nodes.rs:56`)
  registers `PendingWait { arc_id, wait_id, signal, correlation, notify, resolved }`
  in `WaitStore` (`src/workflow/wait.rs:124`), then `tokio::select!`s on
  `notify.notified()` / cancel / timeout (`:157`). The `notify.notified()` is the
  await; the arc task is suspended.
- **The completion packet.** `signal_arc_dispatch` (`src/server/routes.rs:2291`)
  is the completion port: `match_and_take(signal, correlation)` (`wait.rs:141`)
  pops the matching waiter, writes the `SignalRef` into its `resolved` slot, and
  calls `notify_one`. The correlation tuple is the OVERLAPPED key that routes the
  completion to the *right* waiter.
- **Resume from the callsite.** The suspended `select!` wakes, reads the
  `resolved` slot (`wait_nodes.rs:213`), records the signal into `ArcContext`, and
  the arc loop advances to the next node. The "callsite" is the arc's node
  position, owned by the Rust engine.
- **The register-vs-arrive race is already handled.** Before suspending, the node
  does a **catch-up rescan** over persisted `system_events`
  (`wait_nodes.rs:118-148`): if the signal already arrived, it resolves
  immediately. This is the piece that makes the model durable across the gap
  between "I dispatched the child" and "I parked."

### 4.1 Child-bro completion is already a correlated signal

The single biggest finding for this thesis: a durable cell that dispatches a
child bro and parks on it needs **almost no new plumbing**, because bro
completion is already routed onto the signal bus.

- `spawn_task_completed_router` (`src/server/background.rs:90`) subscribes
  `tail_tx` and, on `TailEvent::TaskCompleted { task_id, source_session,
  task_kind }`, dispatches a routed event `"task-completed"` carrying `{task_id}`.
- `spawn_system_event_signal_bridge` (`src/server/background.rs:131`) subscribes
  the *entire* system-event bus and turns **every** event into a candidate
  signal: `signal = event.kind.to_wire()`, `correlation = event.correlation`,
  fed straight to `signal_arc_dispatch` whenever a matching wait exists.

So a durable cell does:

```js
const result = await bro.exec({ brofile: "reviewer", prompt });   // returns a Promise; await parks the bro
```

`bro.exec` returns a **Promise** (the wait token). Awaiting it **parks the bro and
returns** — the turn ends — and the existing task-completed router + signal bridge
deliver the settle that wakes the *next* turn (replayed with the settle payload +
persisted KV state). Child-bro completion is just one **producer** of a settle
(timer / signal / webhook are others). There is **no `narf.wait` verb** and no new
completion channel — the only new work is lifting the engine's park/resolve
lifecycle up from authored workflow arcs to bros/cells.

There are already three signal ingress points the durable cell inherits for free:
`bro_arc_signal` (manual, `tools/orchestrate.rs:319`), webhook
(`routes.rs:614`), and the system-event bridge (`background.rs:155`).

### 4.2 The V8 continuation wall — sidestepped exactly as workflows sidestep it

V8 cannot serialize a paused continuation, so a durable cell **cannot** be a JS
function that literally suspends mid-body across a daemon restart. It does not
need to. The engine — Rust — owns the loop and the arc position; the cell body is
re-entered at its await/park boundaries. This is the same trade workflows make:
the durable unit is the *arc state* (current node + `ArcContext`), persisted; the
*body* is replayed/re-entered, not frozen. The continuation wall only bites if
you try to make V8 the durable actor; here V8 is "a turn," and the daemon owns
the state machine — the exact phrasing from the workflow-orchestration SM ("the
LLM stops cosplaying a state machine; the daemon owns the loop"), now applied to
the cell body instead of the model.

The honest cost this imposes: a durable cell body must be **structured around its
park points** — between two park-on-promise-settle points, the work must be either idempotent or
KV-checkpointed, because re-entry after restart re-runs from the last persisted
node, not from the exact JS statement. This is the determinism discipline a
`registerWorkflow` cell signs up for and an `exec` cell does not.

## 5. The two surviving daemon services

Once cells are the substrate, the daemon keeps exactly two services that the
sandbox cannot own itself (because they outlive any one isolate):

1. **The durable park/resolve executor.** `WaitStore` + `signal_arc_dispatch` +
   the arc loop + the catch-up rescan + the signal ingress bridges. ~80% present
   today (§7 names the gap).

2. **The contract/catalog registry.** Where registered/durable cells, their
   contracts, versions, and supersession live — the existing prepared-script store
   (`bro-script/src/lib.rs:138`) graduated into the `bbox_artifact_*` catalog
   (which already has `atom`/`workflow` kinds). This is what `narf_register`
   writes and what in-box `atom:<name>@<v>` deref reads.

Everything else the atom/workflow systems do today — the four atom backends, the
JSON node graph, the typed `next` edges — becomes *the cell body's business*, not
the daemon's. A "deterministic atom" is a cell with no `effects`. A "profile
atom" is a cell whose body is one awaited `bro.exec`. An "ensemble
workflow" is a cell that fans out N `bro.exec`s and awaits them all (`whenAll`)
(the existing `bro_when_all` semantics, expressed in JS). The taxonomy collapses
into "what the body happens to do."

## 6. What retires (eventually) and what does not

- **Retires (subsumed):** hand-authored JSON workflow node graphs for the common
  shapes (sequence, fan-out/in, gate-then-continue) — a JS cell expresses these
  more directly. The four-backend atom dispatch taxonomy as *distinct runtimes*.
- **Does NOT retire:** the engine itself (it becomes the durable executor service
  of §5). The rule-packet gate machinery (a cell calls it; it stays a daemon
  capability). `bro_orchestrate_run` for genuinely declarative, externally-audited
  state machines where a reviewer wants the graph as a static artifact — a JS cell
  is worse when the *graph itself* is the deliverable. The `atom:` ref kind
  (it is the box-edge-legal "handle" use of ref the data-model doc preserves).

The honest framing: this is a **convergence target**, not a delete list. Atoms
and workflows keep working; new durable/reusable behavior is authored as cells;
the JSON surfaces are retained for the cases where a static graph is the point.

## 7. Gaps against current code (verified by file:line)

The thesis is "mostly built." Precisely what is *not*:

1. **Durable/workflow-tier contract enforcement is not built yet.** The A1/A2
   reusable-cell slices are live: `narf_prepare` accepts an optional declared
   `contract`, validates `entry`, validates `input`/`output` with JSON Schema
   Draft 2020-12, echoes the contract in `PrepareResponse`, and keeps it with
   the prepared script; `narf_register` persists reviewed source+contract as a
   **cell** artifact (`ArtifactKind::Cell`, not an atom backend), and
   registered-handle `narf_run` validates input before calling the declared
   entry and output before returning. What does **not** exist yet is the durable
   workflow-tier enforcement around park/resume boundaries.
2. **Durable handles are not lifted to the bro/cell level.** The live async
   bridge exists for same-activation promises, and the workflow
   `WaitSpec`/`run_wait_node` lifecycle exists for authored arcs. What is missing
   is an explicit durable-handle layer for producers that intentionally outlive a
   live cell: a child bro, long shell job, timer, webhook, or signal. Do not
   implement this as "every `await` parks and wakes a new turn"; ordinary awaits
   should resolve inside the live activation. The build is a daemon-owned
   durable wait/join/resume policy over explicit handles.
3. **WaitStore is not persisted; parked arcs do not survive restart.**
   `wait.rs:121` is explicit ("v1 — no persistence"), and `restore_runtime_state`
   (`src/server/restore.rs:6`) restores webhooks/pollers/crons/whiteboards/
   councils/workflow **specs**/catalog/reactions/outbox — but **not in-flight arc
   run state**. So today the durable tier is durable *within* a daemon lifetime
   (signals persisted as system events + catch-up rescan), **not across a
   restart**. Closing this is the durable tier's one real build cost: persist
   `WaitStore` (the struct is "designed serializable" already) + respawn parked
   arc-runners on boot so they re-enter their wait node and let the catch-up
   rescan re-resolve.
4. **Durable/scheduled cell verbs exist, but not parked continuation
   enforcement.** `narf_registerWorkflow` promotes an exact registered cell
   handle into a durable cell artifact; `narf_scheduleWorkflow` persists a
   cell-native schedule and wakes the exact durable cell directly with the
   schedule payload plus `schedule_name`/`tick_at`. This path deliberately does
   **not** install a workflow graph, workflow hook op, atom backend, routing
   packet, or packet evaluator. What remains absent is the park/resume lift in
   item 2 and cross-restart parked state in item 3.
5. **No TS toolchain.** Restating §1: "typed" is a declared contract, validated;
   it is not an inferred TS signature. If inferred typing is ever wanted, it is a
   separate, scoped toolchain decision — not part of this doc's v1.

## 8. Decisions

- **D6 — Authorial body channel — DECIDED (uniform JSON tool surface; freeform
  grammar deferred).** The cell `source` (and the register/durable verbs' inputs)
  cross as a standard JSON string arg in a normal function-tool schema, **identical
  across every transport** — no freeform/grammar channel and no per-transport
  channel adapter in v1. Correctness rests entirely on Layer B: `prepare`
  syntax-check + (for contracted cells) server-side schema validation + the
  rendered-source review step + a repair turn. The codex-style freeform-grammar
  code-channel (raw JS, no JSON escaping — see `codex-rs/code-mode`,
  `core/src/tools/handlers/apply_patch.lark`) is a **later, Brodex/Responses-scoped
  enhancement** layered onto the same logical surface, not a v1 dependency.
  *Why this is forced, not merely chosen:* a 2026-06-04 adversarial probe of the
  actual fleet found the Anthropic-shaped clones **GLM / DeepSeek / MiniMax all
  silently ignore `strict` and structured-output (`output_config.format`)** —
  e.g. DeepSeek/MiniMax emitted tool args that directly violate a `strict` schema.
  Only Vibe/Mistral (chat `response_format: json_schema` + tool `strict`) and
  Brodex (Responses; freeform grammar + strict) enforce anything. So there is no
  transport-portable constrained-decoding floor to build on — Layer-B server-side
  validation + repair is the *only* mechanism available on most of the fleet, and
  the cheap house-style rules (single-quote-preferring JS, single top-level
  `source` arg, minimal nesting) carry the ergonomic load until the grammar
  enhancement lands.

### Open decisions

- **D1 — Contract schema dialect.** JSON Schema (verbose, standard, already used
  by atoms) vs a thinner bespoke shape. Leaning JSON Schema for atom parity.
- **D2 — Wait authority — DECIDED (explicit durable handles; live awaits stay
  live).** A cell never authors a `{signal, correlation}` selector in-box — that
  is correlation *selection*, which stays out-box. Ordinary JS promises are
  ephemeral live-activation promises. Cross-turn/cross-restart waiting requires a
  daemon-issued durable handle from a producer that is explicitly durable. The box
  may await or join handles it was handed; it never selects what external signal
  to wait on.
- **D3 — Determinism discipline enforcement.** §4.2 asks durable bodies to be
  checkpoint-structured. Is that a lint, a runtime guard (e.g. forbid
  non-idempotent effects between un-checkpointed park points), or just doc
  guidance? Start as guidance; revisit if foot-guns surface.
- **D4 — Where the contract registry physically lives.** Graduate the
  prepared-script store into the `bbox_artifact_*` catalog, or a dedicated cell
  registry? Catalog reuse is cheaper and gets versioning/supersession for free.
- **D5 — Scheduling triggers — DECIDED (cell-native scheduler).**
  `narf_scheduleWorkflow` owns a typed-cell schedule registry and invokes exact
  durable cell handles directly. Existing `crons`/`pollers` and routing packets
  remain for legacy workflow/event ingress; they are not the typed-cell
  scheduling implementation.

## 9. Relationship to sibling docs

- [`narf-data-model.md`](./narf-data-model.md) — the value substrate cells read
  and write (`narf.kv`); `atom:` survives only as a handle, never as data
  composition. This doc's cells are the *producers/consumers* of that KV.
- [`narf-capability-library.md`](./narf-capability-library.md) — the
  authoring ladder and the prepare/run split this doc extends with register/
  schedule verbs and a contract block; §8.1's "typecheck against `.d.ts`" axis is
  the place an *inferred* type story would eventually attach (explicitly out of
  scope for v1 here).
- [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) — topology and the
  `bro-capabilities` traits; the durable executor of §5 is a daemon service the
  harness reaches only through the documented seam, never an RPC backchannel
  (the bro-harness/daemon invariant).
- `system-defaults/memories/workflow-orchestration.md`,
  `system-defaults/memories/atoms.md` — the authoritative current behavior this
  doc proposes to converge; read them before treating any of §6 as already true.

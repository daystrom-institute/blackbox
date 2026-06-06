---
title: "Workflow-JS: composable state machines as the workflow surface"
kind: design
lifecycle: superseded
superseded_by: "codexification.md — bro-harness adopted Codex code-mode (exec/wait); NARF retired"
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - authoring-layer
  - orchestration
  - workflows
  - state-machine
  - durable-execution
brief: "The workflow engine is the last pre-sandbox bespoke surface, and its JSON node graph is already a state machine. Replace it not with another subsystem but with a state-machine LIBRARY exposed to the V8 sandbox (Stateless/ZCrew.StateCraft-shaped: states, transitions fired by triggers, guards, entry/exit/action handlers, and — crucially — child machines). A workflow is a cell that declares a machine against this library, reusable at every scope (ephemeral exec → registered → durable workflow; the verb sets the tier). The machine STRUCTURE is data the daemon validates (every transition targets a declared state, closed literal trigger alphabet, reachable/terminal states, child machines resolve, child-invocation contracts typecheck) and renders to mermaid; the handler BODIES are JS at shell trust (validation is well-formedness, not safety). Composition is child state machines (the ZCrew.StateCraft OrderProcessor pattern): a parent owns N child machines, commands DOWN via state actions, joins on children via WhenAll/WhenAny triggers + guards that read child state, children signal UP on entry to terminal states — subworkflow / ensemble / foreach / fork are all this one pattern. Durability is a tree of independently-persisted {state, KV}; the daemon backs the durable primitives (persisted state, trigger arming incl. joins, child activation, dispatch, restart re-arm) while pure primitives stay JS. Maps keystone/sastquatch. Agents learn it via tooldocs + signposts + a system memory, not always-rendered prose. Inherits trust/box-edge/KV from the siblings; builds no determinism/effects enforcement (parked)."
---

# Workflow-JS: composable state machines as the workflow surface

> Proposed. Completes the durable tier of
> [`narf-typed-cells.md`](./narf-typed-cells.md). Trust
> ([`narf-effects-and-safety.md`](./narf-effects-and-safety.md)), the box edge
> ([`narf-capability-library.md`](./narf-capability-library.md) §0.1), and the
> durable KV ([`narf-data-model.md`](./narf-data-model.md)) are inherited, not
> restated. Reference shapes:
> [Stateless](https://github.com/dotnet-state-machine/stateless) (external state
> accessor; DOT/Mermaid export; substates) and
> [ZCrew.StateCraft](https://github.com/ZCrewSoftware/ZCrew.StateCraft) — its
> `WithAction` / `WithTrigger(.Await(signal).ThenInvoke(transition))` primitives are
> real, and its `samples/OrderProcessorSample` parent/child pattern (hand-composed via
> owned child contexts + `Task.WhenAll`/`WhenAny` — **not** a built-in StateCraft
> construct) is the pattern NARF makes first-class in §4–§5. Grounded against
> `beta/blackbox-v2`; verify against code.

## 1. The idea

The workflow engine predates the V8 sandbox, so it invented a JSON node graph
(`goto`/`branch`/`fork`/`terminal` + `wait` nodes, actuated by a routing packet).
**That node graph is already a state machine.** The sandbox lets us keep the state
machine and throw away the JSON — not by building another bespoke workflow subsystem,
but by exposing a **state-machine library** to the cell sandbox. A workflow is a cell
that declares a machine against that library; the declared structure stays *data* the
daemon validates and renders, while the entry/action/guard bodies are *JS*, so the
whole hook-op vocabulary dissolves into ordinary code. The same library is **reusable
at every scope** — an ephemeral `narf_exec` cell, a registered cell, or a durable
workflow — with the verb setting the durability tier (typed-cells). The agent declares
a machine; the daemon runs it.

## 2. The library

A workflow cell's top-level body declares states and the triggers that move between
them. Keystone (`examples/keystone/`) as a machine:

```js
narf.workflow('issue-to-merged-pr', wf => wf
  .initial('Implement')

  .state('Implement', s => s
    .onEntry(async ctx => {
      ctx.kv.set('branch', `fix/issue-${ctx.event.issue_number}`); ctx.kv.set('iter', 0);
      await ctx.worktree.create();
      ctx.dispatch('implementer', { issue: ctx.event.issue_number });   // leaf bro; completion is a trigger
    })
    .onDispatch('implementer', (ctx, r) => { ctx.kv.set('pr', r.pr_number); return ctx.go('AwaitReview'); }))

  .state('AwaitReview', s => s
    .on('pr-ready',  ctx => ({ pr: ctx.kv.get('pr') })).go('Review')
    .on('pr-merged', ctx => ({ pr: ctx.kv.get('pr') })).go('Done')
    .timeout('24h').go('Done'))

  .state('Review', s => s
    .onEntry(ctx => ctx.dispatch('reviewer', { pr: ctx.kv.get('pr') }))
    .onDispatch('reviewer', ctx => ctx.go('AwaitFeedback')))

  .state('AwaitFeedback', s => s
    .on('pr-feedback').guard(ctx => ctx.kv.get('iter') < 5).go('AddressFeedback')
    .on('pr-feedback').go('Done')
    .on('pr-merged').go('Done')
    .timeout('7d').go('Done'))

  .state('AddressFeedback', s => s
    .onEntry(ctx => { ctx.kv.inc('iter'); ctx.dispatch('feedback', { pr: ctx.kv.get('pr') }); })
    .onDispatch('feedback', ctx => ctx.go('AwaitReview')))

  .state('Done', s => s.terminal()
    .onExit(async ctx => { if (ctx.outcome === 'success') await ctx.worktree.remove(); })));
```

The primitives — each a StateCraft/Stateless analog (except child composition, §4,
which NARF adds) replacing a node-graph concept:

| Primitive | Replaces | Notes |
|---|---|---|
| `onEntry` / `onExit` | hook nodes / `on_enter`/`on_exit` | JS bodies; the hook ops are now `ctx.kv` + host tools |
| `.on(trigger, corr?).go(target)` | `wait` node + `next` edge | external signal trigger; optional correlation extractor |
| `.guard(ctx => bool)` | gate packet | predicate on a transition; first-pass-wins among same-trigger transitions |
| `.timeout(d).go(target)` | `__timeout__` | StateCraft `WithTrigger(.Await(deadline))` |
| `.onDispatch(name, (ctx,r)=>…)` | actor node completion | a *leaf* bro dispatch; its completion is a trigger (StateCraft `WithAction`) |
| `.child(...)` / `onAll`/`onAny` join | `subworkflow_ref`, `fork`, `foreach`, ensemble | §4 — **NARF-native**, not a StateCraft primitive |
| `.terminal()` | `terminal` | no outbound transitions |

The daemon snapshots the declaration and renders it — the auditable artifact the JSON
graph had, regenerated, identical to the diagram keystone's README hand-drew:

```
stateDiagram-v2
  [*] --> Implement
  Implement --> AwaitReview: implementer done
  AwaitReview --> Review: pr-ready
  AwaitReview --> Done: pr-merged / 24h
  Review --> AwaitFeedback: reviewer done
  AwaitFeedback --> AddressFeedback: pr-feedback [iter<5]
  AwaitFeedback --> Done: pr-feedback / pr-merged / 7d
  AddressFeedback --> AwaitReview: feedback done
  Done --> [*]
```

## 3. Validation

The machine *structure* is data, not opaque JS, so the daemon validates it at
register/prepare time — before anything runs (StateCraft does this: "detects duplicate
states and invalid transitions"):

- every transition targets a **declared** state — no dangling gotos;
- the trigger alphabet is **closed after registration** — the snapshot fixes exactly
  the triggers the registration pass declared, so a machine cannot listen for more than
  it registered (the box edge). Proving the names are *literal* (`'pr-ready'`, not
  `computeName()`) needs source-level static analysis — a checker we can add, not a
  property the runtime builder gets for free from ordinary JS strings;
- terminal states (explicitly `.terminal()`) have no outbound edges; every non-terminal
  state has at least one outbound path (a trigger, an action transition, or a child
  completion); all states are **reachable** from the initial state — no orphans;
- referenced **child machines resolve** (the composition graph closes, §4);
- **child-invocation contracts typecheck** against the child cell's `CellContract`, in
  three shapes (§4): a single child (input projection in, validated output on terminal
  — the `subworkflow_ref` imports/exports), an N-child **collection** result (the
  ensemble/`foreach` join — an ordered/labeled array + partial-failure shape), and a
  **deferred** child (`late_inject`);
- guards/handlers **parse**.

This is the static, inspectable artifact the node graph gave and a bare JS-handler
model lost — and stronger, because a real state machine has well-formedness properties
a node graph doesn't. It also answers D6 (typed-cells §8): the source still crosses as
a JSON string, but **what executes is a validated machine, not an unvalidated blob** —
Layer-B validation now has a rich structure to check, and the rendered machine is what
the model reviews before launch.

**Validation is well-formedness, not safety.** The daemon checks the *machine*
composes and resolves — a cheap, compiler-shaped check. It does not gate what the
handler *bodies do*; those are arbitrary JS at shell trust, ungated (the trust model,
§7). A dangling transition is a bug the daemon catches; an `rm -rf` in a body is not
its business.

## 4. Composition: child state machines

A state's body can be a **child machine**, and a parent can own *N* of them. This is
**NARF's first-class composition primitive, not a StateCraft library feature** —
StateCraft has no built-in nested/substate construct; its `OrderProcessor` sample
composes child machines *by hand* (`OrderContext` owns a `List<LineContext>`, activates
each manually, and coordinates them through app-owned `AsyncManualResetEvent`s +
`Task.WhenAll`/`WhenAny`). NARF lifts that hand-rolled pattern into the library
(`.child`/`onAll`/`onAny`, host-backed). The lifted pattern is the single answer to
subworkflow, ensemble, `foreach`, and `fork`:

- **Command DOWN** — the parent drives children from a state action (`OrderContext`'s
  `Suspending` state `WithAction(SuspendLines)` iterates the line machines). Fan-out =
  spawn N children from a runtime collection (`order.Lines.Select(...)`), the dynamic
  `foreach` a fixed graph can't name as states.
- **Signal UP** — a child sets a completion signal on entering a terminal state
  (`OnEntry(() => LineClosed.Set())`).
- **Join** — the parent arms a trigger that awaits the children:
  `WithTrigger(.Await(WhenAll(children)).ThenInvoke(transition))` is the durable
  whenAll; `WhenAny` is any-of. Guards read child state (`AllLinesCompleted()`), so the
  fan-in *condition* is a guard.

So the reviewer **ensemble** is a parent `Review` state that spawns N reviewer child
machines, joins on all-complete, then transitions to `Aggregate`:

```js
.state('Review', s => s
  .onEntry(ctx => ctx.kv.set('kids', ctx.reviewers.map(r => ctx.spawn('reviewer', { pr: ctx.kv.get('pr'), who: r }))))
  .onAll('reviewer', (ctx, results) => { ctx.kv.set('verdicts', results.map(r => r.verdict)); return ctx.go('Aggregate'); }))
```

`onAll` delivers results **ordered and labeled by child id / team member** (with a
partial-failure shape), so an aggregator reproduces the engine's `${Review.output}` (a
labeled concatenation, one block per reviewer) rather than an unordered bag.

`subworkflow_ref` is one child (`ctx.spawn` + `onDone`); ensemble/`foreach` are N
children + `onAll`/`onAny`; `fork`/`fire_and_forget` spawn without a join;
`late_inject` is a transition that fires when a deferred child settles and folds its
result. **A child machine is itself a registered cell**, so composition rides the
capability ladder (cell-local → session → atom) — the same library builds a small
reusable machine and a big one that nests it. The child's `CellContract` is the
imports/exports boundary (input projection in, validated output on terminal). Boundary
policy (`policy_packet` halt/escalate/warn) is a machine-level `wf.policy(fn)`
interceptor on every transition, distinct from a `.guard()`; `wf.onError`/`wf.onCancel`
cover any-terminal/cancel cleanup that a terminal `onExit` does not reach.

## 5. Durability and the host binding

An instance is a **tree of independently-persisted `{ state, KV }`** — the parent
machine plus any active child machines, each persisting its own current state
(`OnStateChange` in OrderProcessor persists every machine after every transition). The
"state vector" is just N+1 normal rows; restart reloads each and re-arms the
transitions its state permits. Armed waits are not separately stored — they are
*derived* from each machine's current state.

The library has two halves, and only one needs the daemon:

- **Pure primitives** — states, guards, internal transitions, the builder — are plain
  JS; a purely in-isolate machine (an ephemeral `narf_exec` cell) needs no host.
- **Durable primitives** are **bound to the V8 host**: persisted `{state, KV}` per
  machine, trigger arming (external signals, dispatch/child completions, joins,
  timeouts), child-machine activation, `ctx.dispatch`, and restart reload/re-arm. The
  daemon backs these for the durable tier.

Same API at every scope; the host backing is the durable tier's seam (the boundary
doc's "consolidation changes the capability seam, not the authoring model"). The
**host-binding contract** is small: `persist(machine, state, kv)`; `arm(machine,
triggers)` incl. join/await-all/any over child instances; `activate(child)`;
`dispatch(bro) → handle`; `reload+rearm` on boot. Producer restart-safety is
per-source and partly new: a completed dispatch is durable because the machine
transitioned past it; an in-flight dispatch at restart needs its producer to re-settle
— a task-store terminal scan re-emitting completions keyed to the handle (new;
`spawn_task_completed_router` only reacts to live `TailEvent`s, `background.rs:90`), a
durable deadline scheduler for timers, upstream redelivery + `delivery_id` dedup for
webhooks. Transition firing is idempotent. Today none persists — `WaitStore` is
in-memory (`wait.rs:121`), `restore_runtime_state` restores workflow *specs* not
in-flight state (`restore.rs:6`).

## 6. Actuation

An event arrives — webhook, cron, signal, dispatch/child completion. The daemon
extracts the instance key (a source-side correlation selector on the webhook spec —
the general extractor dies, bodies shape the raw payload in JS) and asks: *is a live
instance whose current state arms a transition for this `(trigger, correlation)`?*
No instance + a machine subscribes → **cold-start** (instantiate at the initial state,
run its `onEntry`); live instance armed → **route** (fire the transition); nothing →
**dead-letter**. Several machines on one trigger **fan out**; selection is transition
guards (first-pass-wins) + correlation exactness — no priority primitive.

The `wait_store` **matching kernel** is reused (subset-correlation `match_and_take`,
catch-up-on-register); the **delivery is new** — `signal_arc_dispatch`
(`routes.rs:2311`) today `notify`s a parked task through a runner-local `Arc<Notify>`
(`wait.rs:108`), presuming a frozen stack we don't have; a match instead fires a
machine transition. `cancel` (`cancel_arc`, `routes.rs:629`) trips the machine's
cancellation token: stop a running body, drop the instance + its children, cancel
daemon-owned child dispatches; partial KV is left as-is — best-effort, never a
transactional unwind.

## 7. What dissolves, and trust

| Was | Is |
|---|---|
| JSON node graph (`goto`/`branch`/`fork`/`terminal`, wait nodes) | the declared machine (validated + mermaid) |
| hook ops (`set_var`/`http_json`/`find_first`/`parse_json`/`mcp_call`/`worktree_*`) | JS in `onEntry`/`onExit` over `ctx.kv` + host tools + `mcp.*` |
| gate packets (`gate_mode: first`) | transition `.guard()` (first-pass-wins); `gate_mode: all` aggregate gates and `policy_packet` halt/escalate/warn → a machine-level `wf.policy()` interceptor, not a branch guard |
| actor kind `executor` | `ctx.dispatch(bro)` leaf — only executor/ensemble carry semantics; advisor/planner/triager/user are persona labels collapsing to executor (`docs/workflows.md:188`) |
| actor kind `ensemble` | a parent state with N reviewer child machines + a join (§4) |
| `subworkflow_ref` + imports/exports/`import_renames`/`foreach`/`matrix`/`fork`/`late_inject` | child machines + join (§4); contract = the child cell's `CellContract` |
| routing-verdict dispatcher | the §6 instance lookup |

Keystone and sastquatch map onto this: setup → leaf dispatch → wait-on-PR-signal →
guard-branch → loop back-edge → terminal cleanup, with the reviewer ensemble as a
child-machine fan-out. Sastquatch's inversions are facets of the same machine (cron
cold-starts it, `mcp_call` is `mcp.biofilter.sast_run()` in an `onEntry`, the triager
is a `ctx.dispatch` with a selection contract).

**Trust** is inherited from [`narf-effects-and-safety.md`](./narf-effects-and-safety.md)
§0: trusted, attended, single-user box. No safety/determinism/effects *enforcement* is
built — and a state machine needs none (no replay to keep deterministic). `effects` is
elided (**supersedes [`narf-typed-cells.md`](./narf-typed-cells.md) §1.2**); the
`CellContract.effects` field is a hint. Validation (§3) is well-formedness, not a gate.

## 8. What's new to build

| Mechanism | Extends / replaces |
|---|---|
| The state-machine library (builder + pure primitives) | new in-box bindings |
| Structure validator + mermaid renderer | new (the §3 artifact) |
| Daemon machine-runner (state in KV, arm transitions, fire on trigger, run guard/entry/exit) | **replaces** `signal_arc_dispatch` delivery + the node-graph runner |
| Child-machine activation + join triggers (await-all/any over child instances) | new (§4) — the durable whenAll |
| `ctx.dispatch` durable handle + completion-as-trigger | **migrates** `spawn_task_completed_router` off the packet-routed path |
| Durable instance store: a tree of `{state, KV}` + boot reload/re-arm | new; extends `restore_runtime_state` |
| Producer re-settle (task-store terminal scan, durable deadline scheduler) | new (§5) |
| Durable KV wiring | replaces `run_cell_once`'s throwaway `KvStore::default()` (`cells.rs:380`) |
| Reused: correlation-matching semantics + catch-up pattern | `match_and_take` predicate only — not its runner-local delivery |

The legacy engine runs until cutover; this is a migration, not a reskin.

## 9. The agent-facing surface

The library is taught the way every capability is — **not** as always-rendered prose
(render hygiene): a `sm-state-machines` system memory (the idioms, the child-machine
composition, the scopes, the validation rules), `narf_*` **tooldoc** pointers, and
route-card **signposts** (capability-library §3 — "to author a durable multi-step flow,
reach for the state-machine library"). Agents discover it through `atom_search` /
tool descriptions and reuse published machines (registered child cells) by handle.

## 10. Relationship

- **Completes** [`narf-typed-cells.md`](./narf-typed-cells.md) (the durable tier);
  **supersedes** its §1.2 effects-as-grant clause; the library is reusable at the verb
  tiers it defines.
- **Builds on** [`narf-data-model.md`](./narf-data-model.md): the durable KV holds each
  machine's working state + current-state value; separate from `ArcContext.vars`.
- **Inherits trust** from [`narf-effects-and-safety.md`](./narf-effects-and-safety.md);
  enforcement stays parked there. Validation (§3) is well-formedness only.
- **Refines** [`harness-daemon-boundary.md`](./harness-daemon-boundary.md): the
  durable-side store is the instance tree; the daemon machine-runner + host-binding
  contract are new primitives the topology requires.
- **Rides** [`narf-capability-library.md`](./narf-capability-library.md): child
  machines are reusable cells on the cell-local → session → atom ladder; discovery via
  its §3 signposts.
- **Subsumes** the node-graph runtime + routing-verdict dispatcher; **retains**
  `bro_orchestrate_run` only where a static graph artifact is the deliverable (now
  weaker — the library renders one) and the `atom:` handle.
- **Converges** `docs/workflows.md` + `examples/{keystone,sastquatch}` — read those
  before treating §7 as already true.
- **Hub:** [`bro-harness.md`](./bro-harness.md).

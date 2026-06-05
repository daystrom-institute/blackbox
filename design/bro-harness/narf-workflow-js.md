---
title: "Workflow-JS: workflows as reactive cells (the engine subsumption)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - bro-harness
  - narf
  - authoring-layer
  - orchestration
  - workflows
  - webhooks
  - durable-execution
  - box-edge
brief: "Completes the NARF subsumption thesis for the LAST pre-sandbox bespoke surface: the workflow engine. Where narf-typed-cells.md collapsed atoms+workflows into one authorial unit (the cell, verb sets the tier), this doc says HOW a durable workflow is authored and driven once the sandbox is the primary surface — by embracing the embedded-host pattern every game Lua / CEF / mobile-JS runtime converges on: the daemon owns the loop and lifecycle; the cell is a registry of event handlers it fires. A registered workflow is a TEMPLATE: a declared JSON trigger manifest (the closed alphabet of cron/webhook/signal names it reacts to) plus a handler table (code). Instancing it produces a live arc whose per-instance container is shaped like today's ArcContext, handed to each handler as `ctx`; durable instance state lives in the durable KV (a separate store from ArcContext.vars), never in a frozen V8 stack. The durable unit (instance ctx + a PURE armed-wait tuple + an ingress cursor) persists and re-arms on boot — which is what makes the ACTOR STATE survive restart. In-handler effects are at shell-trust parity (a restart mid-`git push` double-pushes exactly as the shell does today; netted by git + operator attention, never by an enforced checkpoint layer). The WaitStore/signal_arc_dispatch correlation-matching + catch-up KERNEL is reused; its delivery (notify a parked task) is REPLACED by handler invocation. The webhook routing VERDICT dissolves into instance-armed-handler lookup; the routing-packet/node-graph/4-atom-backend surfaces become 'what a cell body does.' TRUST MODEL (load-bearing): trusted, attended, single-user YOLO box — this doc builds NO safety/determinism/capability enforcement; durability guarantees are about not LOSING state, never about constraining a handler. Determinism + correlation breadth are authoring GUIDANCE, not machinery (building either would be the parked Tx/saga/effects theater). Grounded file:line against beta/blackbox-v2; supersedes typed-cells §1.2 on effects-as-enforced-grant."
---

# Workflow-JS: workflows as reactive cells (the engine subsumption)

> **Status.** Proposed; converged from a live design jam + an adversarial review,
> grounded against code on `beta/blackbox-v2`. This is the synthesis sibling that
> finishes the job [`narf-typed-cells.md`](./narf-typed-cells.md) starts:
> typed-cells argues the atom + workflow *systems* collapse into one cell and the
> verb sets the tier; this doc specifies the **durable/reactive tier concretely** —
> how a workflow is authored as a cell, how cron/webhook/signal events actuate it,
> and exactly which existing seams it reuses vs. replaces. It sits on top of
> [`narf-data-model.md`](./narf-data-model.md) (the durable KV is the instance
> state), [`narf-capability-library.md`](./narf-capability-library.md) (the
> box-edge invariant + prepare/run authoring split), and
> [`harness-daemon-boundary.md`](./harness-daemon-boundary.md) (topology, the
> `bro-capabilities` traits, the durable-executor placement). Treat §1–§9 as the
> target; §11–§13 are the honest gap/deletion/build ledger against current code,
> verified by file:line. Verify against code before relying on any line here.

## 0. Thesis

> The workflow engine was authored **before** the V8 in-process sandbox existed,
> so — like atoms and packets — it invented its own bespoke authorial surface: a
> JSON node graph with typed `next` edges and `wait` nodes, driven by a routing
> packet that emits `start_arc`/`signal_arc` verdicts. The sandbox makes that
> surface redundant. The claim of this doc: **a durable workflow is a registered
> cell that declares a reactive surface, and the daemon — not a node graph —
> owns the loop.** This is the inversion of control that every embedded-script
> host converges on (game Lua, CEF, React Native): the host drives the lifecycle;
> the script is a *registry of handlers* it calls back. The cell's call stack is
> never persisted across a host lifecycle event — durable instance state lives in
> the durable KV, handlers are short re-entrant invocations over it. That single
> discipline makes the **actor state** (the persisted KV + the set of armed
> waits) survive restart, and dodges the V8 continuation wall, because there is
> never a frozen coroutine to restore.

### 0.1 Trust model — and why this doc builds no enforcement

This is load-bearing, because it governs every "fix" below. The threat model is
[`narf-effects-and-safety.md`](./narf-effects-and-safety.md) §0: **trusted,
attended, single-user YOLO box.** A cell is arbitrary code at the same trust as
the shell the agent already holds, so this doc builds **no** safety, determinism,
or capability *enforcement*:

- **Durability guarantees are about not LOSING state, never about constraining a
  handler.** "Restart-proof" here means the actor's persisted state (KV + armed
  waits) survives a restart — a *correctness/durability* property. It does **not**
  mean a handler's *effects* are transactional. A handler that does `git push`
  after one `await` and is re-fired by a restart double-pushes — **exactly as the
  shell does today.** That exposure is netted the way it already is: local edits by
  git, external effects by operator attention (effects-and-safety §0). Wrapping it
  in a checkpoint gate, effect lint, or mutation guard would be theater — it guards
  a door the authorial surface already leaves wide open.
- **Discipline is authoring GUIDANCE, not machinery.** Where the durable tier
  wants care (idempotent effects between park points; narrow correlations), that is
  advice to the author, enforced by nobody, netted by review. Promoting it to a
  runtime guard is the parked Tx/saga apparatus
  ([`narf-effects-and-safety.md`](./narf-effects-and-safety.md) §1) and stays
  parked until the threat model changes (untrusted or unattended agents).
- **`effects` is elided.** This doc **supersedes
  [`narf-typed-cells.md`](./narf-typed-cells.md) §1.2** on effects-as-an-enforced
  capability-grant: in v1 the `CellContract.effects` field is a review/telemetry
  hint, never a gate. Capability scoping is "what the daemon chose to inject," not
  a self-declared manifest the runtime polices (§8).

The transitional state this replaces is genuinely half-broken (§1): webhooks drive
the *legacy* engine; cells run with a throwaway KV and no host tools; scheduling,
routing, and the wait/resume seam are three disjoint mechanisms.

## 1. Why this exists — the half-built present

Grounded against `beta/blackbox-v2`, three pre-sandbox surfaces coexist with the
cell substrate without being subsumed by it:

1. **Webhooks actuate only the legacy engine.** `POST /webhook/:name`
   (`routes.rs:295`) → `process_webhook` (`:496`) → extractor → routing packet →
   `RoutingVerdict` (`src/routing.rs:36`) → `dispatch_verdict` (`:584`). The two
   arms that actuate work — `StartArc` (`:662`) and `SignalArc` (`:602`) — resolve
   against `state.workflow_registry` (a `HashMap<String, workflow::Workflow>`, the
   JSON node graph) and `state.wait_store`. **No cell path exists.** Grepping
   `wait_store` / `signal_arc_dispatch` / `run_wait_node` across `cells.rs`,
   `orchestration/`, `bro-script`, `bro-harness` returns nothing.

2. **Cells run degraded.** The durable/scheduled cell path `run_cell_once`
   (`cells.rs:380`) builds `tools: None` (`:400`) — a scheduled cell has no
   `fs`/`search`/`git`/`shell`/`web`/`mcp` — and `kv: KvStore::default()` (`:401`),
   a **fresh empty KV every tick**, so the durable-KV-survives-restart property
   (data-model §6) is unrealized on the one path that most needs it. Each tick
   spins a fresh isolate and re-runs `entry(input)` from scratch.

3. **Three disjoint actuation mechanisms.** Cron lives in `CellScheduleRegistry`
   (`cells.rs:201`) + `spawn_schedule_loop` (`:317`). Webhook/signal lives in the
   `wait_store`/`signal_arc_dispatch` seam used only by `run_wait_node`. The
   routing packet (`src/packets/`) is a fourth bespoke surface (an
   antecedent/consequent rule AST) wired as the webhook dispatcher.

The result is the "weird crufty half-broken transitional state": the parts to
unify exist, but the cell is a one-shot function bolted beside the old engine, not
the substrate the old engine dissolves into.

## 2. The embedded-host pattern (inversion of control)

Every embedded-script runtime that survives contact with durability converges on
the same shape:

| System | Host owns | Script provides | Bridge |
|---|---|---|---|
| **Love2D** | the frame loop / lifecycle | `love.update(dt)`, `love.draw()` | well-known global callbacks |
| **WoW / Garry's Mod** | the event bus | `frame:SetScript("OnEvent", fn)` / `hook.Add(name, id, fn)` | `RegisterEvent` + C API |
| **CEF** | navigation / process loop | DOM/event listeners | `CefV8Handler`-blessed natives + message router |
| **React Native / Hermes** | the UI/event thread | component logic | the native bridge / JSI |

Two laws hold across all of them, and both are load-bearing here:

- **The host drives; the script reacts.** The script registers handlers; the host
  invokes specific handlers on events with host-provided arguments. The script
  never owns the main loop.
- **The script's stack is never persisted across a host lifecycle event.** State
  lives in host-owned objects (the game world, the DOM, the redux store), not in a
  suspended coroutine. "What happens next" is *the next event arriving and the
  handler reading persisted state* — not a frozen call stack resuming.

blackbox already has the *downward* half of the bridge (`globalThis` capability
injection + `__bb_host_call`, `bro-script/src/lib.rs:600`; deleted ambient
globals; a `SupervisionPolicy` per-invocation budget). The missing primitive is the
*upward* call: **daemon → cell, "invoke handler `X` with these args in a fresh
activation."** This is **new** (§11) — it is more than today's `call_cell`
(`lib.rs:1395`), which validates and invokes a *single* `contract.entry` from a
wrapped source; the reactive model invokes a *named handler from a table*, which
needs handler-key validation and a per-invocation lifecycle (§7).

## 3. The template / instance model

A **registered workflow is a template**, not a running thing. Instancing it
produces a live arc. The per-instance container is *shaped like* today's
`ArcContext` (`src/workflow/context.rs:29`), which already threads through every
wait node and gate — strong evidence the model is natural rather than a rewrite.
But two of its fields are not what they look like, and the doc must not conflate
them (a v1-review correction):

| `ArcContext` field (`context.rs`) | Cell `ctx` role |
|---|---|
| `vars: Map<String,Value>` | instance **variable shape** — schema-validated workflow state with hook write semantics (`context.rs:149-177`). It is **NOT** the durable KV. The durable KV (`narf-data-model.md`; the harness `KvStore`) is a **separate store** with its own API; wiring it as the persisted instance KV is a build item (§11), not "respawn `vars`". |
| `outputs` / `actor_results` | **collapse** — no node graph ⇒ no per-node bookkeeping; a dispatch's result is what its handle resolves to. |
| `meta.arc_id` (`ArcMeta:52`) | the **instance id** |
| `meta.workflow_name` / `version` | the **template** this instance was struck from |
| `meta.project_dir` / `worktree` | `ctx`'s root; the cwd every `ctx.tool` / `ctx.dispatch` inherits |
| `meta.parent_arc_id` / `composition_depth` | the **recursion-guard / ancestor-budget tree**, already modeled — `ctx.dispatch` inherits depth/budget unchanged |
| `last_signal: Option<SignalRef>` | the **event** arg handed to the fired handler |
| `signal_history: Vec<SignalRef>` | a readable durable **event log** on `ctx` |

So the durable unit is **`instance = persisted (ctx-shape + durable KV + armed
waits + ingress cursor)`**; the template is the immutable `(source, manifest,
handler contracts)`. `ArcContext` is the right *shape* for the instance metadata
and signal history; the durable KV is a distinct store it carries a reference to.

## 4. The authorial surface: a declared manifest + a handler table

A registered-workflow cell has two parts, split on a hard line (§6):

```js
// ── the cell SOURCE: handler bodies + helpers. The daemon invokes named
//    handlers on events with the persisted instance ctx. ──
export const on = {
  async 'github.pr-opened'(ctx, { correlation, payload }) {
    const h = await ctx.dispatch('reviewer', { pr: payload.number });  // DURABLE handle
    ctx.kv.set('pr', payload.number);
    ctx.arm('reviewer-done', { review: h.id });   // arm next handler, then RETURN
  },
  async 'reviewer-done'(ctx, { correlation, payload }) {
    if (payload.verdict === 'approve')
      await ctx.tool('git.merge', { pr: ctx.kv.get('pr') });
    // no re-arm ⇒ this instance is terminal
  },
  async nightly(ctx, { tick_at }) { /* … */ },
};
```

```jsonc
// ── the DECLARED MANIFEST: a JSON field on the cell contract (NOT a JS export).
//    The daemon reads it to wire subscriptions WITHOUT executing handler code,
//    and it is what the model reviews before launch. ──
"manifest": {
  "triggers": {
    "webhooks": [{ "signal": "github.pr-opened", "guard": "cell:pr-policy@v3" }],
    "crons":    { "nightly": "0 3 * * *" },
    "signals":  ["reviewer-done"]            // signal NAMES handlers may arm
  },
  "root_correlation": { "pr": "$.payload.pull_request.number" },  // instance key (§9)
  "handlers": {                              // per-handler keys the daemon may invoke
    "github.pr-opened": {}, "reviewer-done": {}, "nightly": {}
  }
}
```

- **The manifest is a declared JSON contract field, not a JS `export const`.**
  There is no static JS-export inspector — `call_cell` executes source to reach a
  named function (`lib.rs:1395-1419`) and `CellContract` has no manifest today
  (`lib.rs:127`). So the manifest is data the daemon parses without running handler
  code: the subscription set (which webhook signals deliver here, which cron, which
  signal names may be armed), reviewed by the model before launch (the box-edge
  review step, capability-library §0.1). v1: **JSON only** — no computed manifests
  (§13).
- **Handlers are code**, invoked by the daemon (the upward bridge, §2) over the
  persisted `ctx`. The handler body owns all imperative flow — the conditionals
  and "fire next step" the node graph used to encode are now ordinary JS plus
  `ctx.arm`. v1 handlers are `(ctx, event)` async functions named in the manifest;
  per-handler input/output schemas are a later nicety, not v1.

The pseudocode shape an author reaches for —
`function webhook.pr_opened(correlation, payload) { … }` — is the `on['…']`
handler table keyed by manifest name.

## 5. Two await regimes — and the honest restart boundary

The continuation wall (typed-cells §4.2) is real: V8 cannot serialize a paused
coroutine, so a handler **cannot** suspend mid-body across a restart. It does not
need to, because there are two await regimes and only the durable *state* survives:

- **In-handler `await` — live-async, ephemeral (the coroutine regime).** Ordinary
  `await fs.read()` / `await atoms.invoke()` is the raw-V8 live-async boundary
  (narf-typed-cells §4.0): the host callback stores a `PromiseResolver`, Rust
  resolves it into the same activation. It is Lua's `coroutine.yield` — ergonomic,
  linear, and it **dies with the isolate**. A restart mid-handler **re-fires the
  handler from the top.**
- **Cross-handler durability — `arm` + KV (the actor regime).** A producer that
  intentionally outlives the activation (`ctx.dispatch` of a child bro, a long
  shell job, a timer, a webhook) **returns a handle and the handler returns**; the
  next step is a *different handler the daemon fires when the result arrives*. The
  only state that crosses the boundary is the durable KV the handler explicitly
  wrote and the armed-wait tuple.

**What "restart-proof" does and does not mean.** The actor *state* — the persisted
KV + the set of armed waits — survives restart, because there is no frozen stack to
restore, only data to reload and re-arm (§9). The handler *body* does **not** get
exactly-once semantics: a restart mid-handler re-fires it, so an irreversible effect
performed before the handler returned can repeat. **This is identical to the shell
the agent already holds** — a restart mid-`git push` double-pushes today — and it is
netted identically: local edits by git, external effects by operator attention
(§0.1). The durable tier therefore carries an **authoring guideline**, not an
enforced contract: between two handler returns, prefer idempotent or KV-checkpointed
effects, and push irreversible effects to the start of a handler that does nothing
else durable after them. We deliberately build **no** checkpoint gate, effect lint,
or "durable-handler-may-not-await-effects" rule — that is the parked Tx/saga
apparatus (§0.1, effects-and-safety §1), and on a trusted attended box it guards
nothing the shell does not already expose. If the threat model ever changes, the
enforcement lever un-parks there, not here.

## 6. The box edge: a static alphabet, dynamic correlations

`ctx.arm(signal, correlation)` takes two different *kinds* of argument:

- **`signal` is a name from a closed, statically-declared alphabet**
  (`manifest.triggers.signals`). It is the *type* of event; the complete set a
  template can ever wait on is finite and known without running a handler.
- **`correlation` is a runtime value** (`{review: "abc-123"}`) — *which instance*
  of that event type, computed during execution. Pure data.

**The cell declares its alphabet statically; it binds letters to instances
dynamically.** Names are types; correlations are values. Three reasons the *name*
side must be static:

1. **It is the box-edge invariant applied to subscription *discovery***
   (capability-library §0.1). Computing a *signal name* at runtime —
   `arm(payload.someField, …)` — is *selecting what classes of event to react to*,
   an interpretive authoring act no model reviewed; the same reason `corpus.search`
   and `kv.list` are out-box. Binding a blessed reaction to a specific instance is
   mechanical dereference — in-box-legal. It is the `kv.get(knownName)`-in-box vs
   `kv.list`-out-box line, applied to waits.
2. **The daemon needs the name set statically** to wire ingress and re-arm on boot:
   webhook ingress delivers *by signal name*; the subscription table is built from
   the manifest at register time, without executing the cell. On restart the daemon
   re-arms from persisted `(template, instance, signal, correlation)` tuples —
   **names are the schema, correlations are the rows.**
3. **Auditability.** A reviewer reads the manifest and knows the *complete* set of
   external event names that can wake any instance — a closed, inspectable surface.

**On correlation breadth — an honesty note, not an enforcement hook.** Current wait
matching is subset-semantics and empty-correlation is broadcast (`wait.rs:84-103`),
so a handler that arms `{}` or omits discriminating keys wakes on more than it
likely meant. This is **not** a box-edge hole the name alphabet was ever claimed to
close — the static guarantee is about what a cell can *discover/select* (names), not
about how tightly a correlation it already holds is scoped. A too-broad correlation
is an **authoring correctness bug** (the instance wakes spuriously), netted by review
and visible in the trace — the same class as any cell logic bug. We do **not** add a
"correlation schema" with required-keys/broadcast-grant enforcement; that is the
effects-grant theater (§0.1) in a new costume. If broadcast arming proves a footgun
in practice, the lightest honest response is a lint/warning in `narf_prepare`, never
a runtime capability gate.

## 7. Actuation: how one event reaches a handler

The `wait_store` **matching kernel** is reused; its **delivery** is replaced. This
is the central review correction: `signal_arc_dispatch` (`routes.rs:2311`) today
matches one `PendingWait` and `notify.notify_one()`s an *already-parked arc task*,
reading a runner-local `Arc<Notify>` + `resolved` slot (`wait.rs:108-118`). That
delivery presumes a frozen Rust task — precisely the thing §5 says we do **not**
have. So:

- **Reused semantics (not the functions):** the correlation-matching predicate
  (subset semantics — `matches_correlation` / `match_and_take`'s match test,
  `wait.rs:84-103/141`), the canonical `(signal, correlation)` wait-tuple shape,
  and the catch-up-on-register pattern (`wait_nodes.rs:118-148`). `match_and_take`
  itself is **not** reusable as-is — it consumes a `PendingWait` and returns the
  runner-local `Notify`/slot; the new store needs its own match-and-claim API over
  serializable armed-handler records.
- **Replaced delivery:** instead of resolving a runner-local `Notify`, a match
  **invokes the armed handler** — a new event-delivery lifecycle: load the
  persisted instance `ctx`, inject the role-scoped caps, run the named handler over
  the event, persist any `ctx`/KV changes and newly-armed waits, then handle
  terminal/GC. The persisted armed-wait record is therefore a **pure serializable
  tuple** `(template, instance, signal, correlation)` — **not** `PendingWait`'s
  `Notify`/slot, which are runner-local and unserializable.

With that, the routing **verdict** (the part that owned the dispatch loop)
dissolves into instance lookup:

| Today (`dispatch_verdict`, `routes.rs:584`) | Workflow-JS |
|---|---|
| `StartArc{workflow,initial_vars}` (`:662`) — explicit workflow id + vars | **Cold-start:** the manifest *subscription index* (built at register, §4) maps `(inlet/signal, guard)` → template(s); a delivery with no live instance instantiates the matching template(s) and runs the trigger handler. |
| `SignalArc{signal,correlate,payload}` (`:602`) — signal + subset correlate | **Route:** instances armed for `(signal, correlate)` get the handler fired over their persisted `ctx`. |
| `CancelArc{correlate}` (`:629`) — trips matching arcs' `CancellationToken` via `cancel_arc` | **Cancel** (more than disarm): drop the instance's future armed waits, mark it cancelled, cancel/ignore its outstanding durable handles, and define the fate of an already-running handler activation (the §11 lifecycle mechanism). |
| `Ignore` / `DeadLetter` | unchanged ingress outcomes. |

**Multi-template / fanout / dead-letter is a real semantic, not "instance
exists?"** (review correction). The cold-start vs route decision must be defined for:
*zero* templates match an inbound signal (→ dead-letter, as today's `no_match`);
*one* template (→ cold-start or route); *many* templates subscribe the same signal
(→ fan out, one instance each — the explicit-consequent selection routing packets do
today moves into the manifest subscription index); *many* live instances correlate
(→ all matching instances fire — subset/broadcast semantics, §6); *guard fails*
(→ no instantiation). v1 default: **fan-out on cold-start, broadcast on route**, with
dead-letter on zero-match, all visible in the delivery trace.

Two internal producers feed the bus but are **migration items, not free**: child-bro
completion currently routes through `dispatch_routed_event("…task-completed-routing…")`
(`spawn_task_completed_router`, `background.rs:90`) — i.e. through the routing-packet
dispatcher this design dissolves — and the system-event signal bridge
(`spawn_system_event_signal_bridge`, `:131`). Lifting `ctx.dispatch`-handle settles
onto the new delivery path means giving task completion a **direct durable-signal
producer** keyed to the handle, rather than inheriting the packet-routed path.

## 8. The subsumption: everything is a cell, differentiated by contract

Once the cell is the substrate, the distinguishing axis is the **contract shape**
(`CellContract`, `bro-script/src/lib.rs:127`: `entry`/`input`/`output`) and the
**tier** (the verb — typed-cells §0), *not* a separate runtime. The bespoke
surfaces become "what the body does" (typed-cells §5):

| Was | Is |
|---|---|
| routing-packet predicate (`packets/ast.rs`) | a cell, `output: {type:boolean}` (or a verdict enum) |
| webhook extractor (`workflow/extractor.rs`) | a cell, `(raw) -> Entity` (or kept as infra — §13) |
| a workflow node / step | a cell, or a handler body |
| an "ensemble workflow" | a handler that `ctx.dispatch`es N and arms on the join |
| the 4 atom backends (profile/workflow/deterministic/adapter) | **dissolved** — `atom:` survives only as an invocation handle (typed-cells §0.1) |

**Predicates** — a predicate *is* a typed cell with a boolean output contract. The
packet AST was a fourth pre-sandbox bespoke surface and collapses by the same logic
as atoms/workflows; the trigger `guard` (§4) is `cell:pr-policy@v3`, a cell — not a
packet handle. A predicate cell is **evaluated like any cell** in v1. The packet AST
may, *later*, survive only as an **invisible compiled lowering** of a pure
predicate-cell contract (a fast-path that dodges the isolate for simple decision
tables) — but it is never an author-visible packet handle or dispatcher, and
**routing packets / packet evaluators are explicitly not part of typed-cell
scheduling or trigger routing** (the Note-23 / D5 invariant in typed-cells). Whether
a guard is "pure" is an *optimization* concern (cheap eval, memoize,
isolate-free-at-ingress), deferred — not a v1 design element, and explicitly **not**
a `tools: None`-style claim: today's `run_cell_once` still injects `atoms` +
`refactor` (`cells.rs:393-399`), so "no host tools" is degraded capability, not
purity. v1 evaluates a predicate as a cell; role-scoped capability bundles
(§0.1 "what the daemon injects") are how a guard *could* later be handed a
compute-only surface, when that optimization is wanted.

**On `effects`:** elided per §0.1; this section **supersedes typed-cells §1.2** on
effects-as-an-enforced-grant. The v1 `CellContract.effects` field stays a
review/telemetry hint.

## 9. The new durable store

§7 names the new delivery path; this is the state it persists. It is **one of
several new mechanisms** (the honest list is §11), not "the only new thing" — but it
is the one with real durability cost, and it is narf-typed-cells §7 items 2/3.

Today `WaitStore` is explicitly `v1 — no persistence` (`wait.rs:121`; its
`PendingWait` holds runner-local `Arc<Notify>` handles, so it is **not** serializable
as-is despite the aspirational comment), and `restore_runtime_state` (`restore.rs:6`)
restores webhooks / pollers / crons / workflow **specs** / catalog — but **not
in-flight arc state**. So the present durable tier is durable *within* a daemon
lifetime, **not across a restart.**

The durable instance store holds, per live instance: the persisted `ctx` shape
(instance id, template ref, project/worktree root, recursion lineage, signal log),
the **durable KV** (the separate store, §3), and the **pure armed-wait tuples**
`(signal, correlation)` (§7 — no `Notify`/slot).

**Closing the register-vs-arrive race needs a durable ingress cursor, not just the
rescan** (review correction). The current catch-up rescan (`wait_nodes.rs:118-148`)
scans recent `system_events` *after* an in-memory wait is registered — but webhook /
direct `signal_arc_dispatch` arrivals are recorded only in the in-memory
`signal_log` (`state.rs`), not a durable, cursored log (some internal producers
already originate in persisted `system_events`). So a webhook arriving while the
daemon is down is not guaranteed replayable. The honest mechanism (the durable
tier's one real cost): **every inbound signal is durably appended to a cursored
event log; arming both records the cursor and, atomically, registers the live armed
tuple, then immediately replays events after that cursor** — closing the *live* race
(an event landing between cursor-read and arm-visible) as well as the restart race;
on boot, re-register the armed waits and replay after each recorded cursor, with
dedup so an event matched live and on replay fires once. If the cursor and the armed
tuple live in separate stores, the arm needs a CAS/transaction boundary so the two
cannot diverge. This is a durability/correctness mechanism (don't *lose* an event),
not a safety gate.

Instance identity is keyed by `(template, root-correlation)` (§7); root-correlation
extraction is part of the manifest/subscription index (§4), with the legacy
Slack-thread seed (`routes.rs:714`) as the precedent for choosing a key from a
multi-field payload.

## 10. The surviving daemon services

After the collapse, the daemon keeps exactly the services a sandbox cannot own
(because they outlive any isolate — typed-cells §5):

1. **The durable park/resolve executor** — the matching kernel + the **new**
   handler-delivery path (§7) + the cursored ingress log. Ingress bridges split:
   webhook + `bro_arc_signal` survive as plumbing, but the **task-completed**
   producer must be **rewired** off the packet-routed path to a direct
   durable-signal producer (§7/§11) — not "all bridges survive unchanged."
2. **The contract/catalog registry** — templates (source + manifest + contracts),
   versions, supersession (the `ArtifactKind::Cell` catalog, `cells.rs`).
3. **The instance store** (§9).
4. **Ingress infra** — webhook signature/dedup/extractor (`webhooks.rs`), cron,
   pollers. Plumbing, unchanged.
5. **The §6 surface evaluator + recursion guard** — capability injection per
   invocation role and the depth/budget tree (`ArcMeta.composition_depth`).

Everything the atom/workflow systems do *beyond* these — the four backends, the node
graph, the typed `next` edges, the routing-verdict dispatcher — becomes the cell
body's business.

## 11. The build ledger (what is genuinely new)

The honest enumeration the first draft undersold as "one new mechanism." Each is a
new build, with its owner and the existing seam it extends:

| New mechanism | Extends / replaces | Notes |
|---|---|---|
| **Manifest reader + subscription index** | new; reads `CellContract.manifest` JSON | wires ingress without executing handlers (§4). |
| **Daemon→named-handler bridge** | generalizes `call_cell` (`lib.rs:1395`) | named-handler lookup + key validation + `(ctx, event)` invocation (§2/§7). |
| **`ctx.arm` lowering** | new; emits a pure `(signal, correlation)` tuple | NOT a `PendingWait` (no `Notify`/slot) (§6/§7). |
| **Handler-delivery lifecycle** | **replaces** `signal_arc_dispatch` delivery | load ctx → inject caps → run handler → persist ctx/KV/arms → terminal/GC (§7). |
| **Instance store** | new; extends `restore_runtime_state` | persisted ctx + KV + armed tuples + cursor (§9). |
| **Cursored ingress event log** | new durable signal/event log (possibly `system_events`-backed, §13) | replaces in-memory-only `signal_log` for replay-after-cursor (§9). |
| **Root-correlation resolver / instance keyer** | new; reads manifest `root_correlation` from the inbound event | resolves the `(template, root-correlation)` key; handles multi-key ambiguity, conflict, fanout (§7/§9). |
| **Instance lifecycle control (cancel / terminal / GC)** | extends `CancelArc` / `cancel_arc` | disarm + mark cancelled + cancel/ignore outstanding durable handles + timeout/GC — distinct from normal terminal completion (§7). |
| **`ctx.dispatch` durable handle** | **migrates** `spawn_task_completed_router` | direct durable-signal producer keyed to the handle, off the packet-routed path (§7). |
| **Durable KV wiring** | extends the harness `KvStore` | a real persisted store as instance KV, not `KvStore::default()` per tick (§3, `cells.rs:401`). |
| **Reused semantics / patterns (no new build)** | `matches_correlation` predicate + subset match, wait-tuple shape, catch-up-on-register pattern | the matching half only; `match_and_take` itself is NOT reused (returns runner-local `Notify`/slot). |

## 12. What retires vs. what is retained

**Retires (subsumed) — all *post-migration*, with the legacy engine retained until
cutover (§13):**

- The **routing-verdict dispatcher** as a control-loop owner (`StartArc`/
  `SignalArc`/`CancelArc` → instance-armed-handler lookup, §7).
- The **JSON node graph** as a *runtime* for the common shapes.
- The **packet AST as an authorial surface** (predicates become cells, §8); at most
  an invisible compiled fast-path survives.
- The **4 atom backends** as distinct runtimes (typed-cells §0.1).
- The **throwaway-KV-per-tick** + `tools: None` degradation of `run_cell_once`.

**Retained (unchanged or lifted, not deleted):**

- The **matching kernel** — `WaitStore::match_and_take` + the catch-up rescan; the
  *delivery* is replaced, not the matching (§7).
- **Webhook ingress infra** (signature/dedup/extractor) — `extractor` may become a
  cell (§8/§13) or stay infra; the signing/dedup plumbing stays.
- `bro_orchestrate_run` for genuinely declarative, externally-audited state machines
  **where the graph itself is the deliverable** (typed-cells §6).
- The **`atom:` handle** (box-edge-legal identifier, never data composition).
- The **recursion guard + supervision policy**, applied per handler invocation.

This is a **convergence target, not a delete-on-merge list.** Existing workflows
keep running on the legacy engine; new durable/reactive behavior is authored as
workflow-JS cells; the routing-verdict dispatcher, node-graph runtime, and packet
authoring surface are removed only *after* cutover, with coexistence boundaries and
adapter shims spelled out at migration time.

## 13. Decisions and open forks

**Decided in this doc (no longer open):**

- **Manifest is static JSON, v1.** §4/§6/§7/§9 depend on it; computed manifests, if
  ever wanted, go behind a separate review/execution mode.
- **Instance identity = `(template, root-correlation)`** with manifest-declared
  root-correlation extraction (§9).
- **Determinism is authoring guidance, not enforced** (§0.1/§5). No checkpoint gate,
  effect lint, or durable-handler effect restriction. The parked Tx/saga lever
  (effects-and-safety §1) is the home if the threat model changes.
- **No correlation-schema enforcement** (§6). Broadcast/subset breadth is an
  authoring correctness concern, at most a `narf_prepare` lint, never a runtime gate.
- **`effects` elided** (§0.1/§8), superseding typed-cells §1.2.

**Genuinely open:**

- **Fan-out vs. select on multi-template / multi-instance match** (§7). v1 default is
  fan-out on cold-start + broadcast on route; whether a manifest may declare
  exactly-one or first-match selection is undecided.
- **Cursored ingress log home** — extend `system_events`, or a dedicated signal log
  with its own cursor? (§9.) Lean: reuse `system_events` (already persisted) + a
  per-instance cursor.
- **Extractor as a cell vs. infra** (§8). Lean: keep signing/dedup infra, allow an
  optional extractor-cell.
- **Per-handler I/O contracts** (§4). v1 handlers are untyped `(ctx, event)`;
  per-handler `input`/`output` schemas are a later nicety.

## 14. Relationship

- **Completes** [`narf-typed-cells.md`](./narf-typed-cells.md): specifies the
  durable/reactive tier — the manifest+handler surface, the actor/instance model,
  the matching-kernel reuse + delivery replacement. Its §7 items 2/3 (durable-handle
  lift + WaitStore persistence) are §9/§11 here. **Supersedes its §1.2** on
  effects-as-enforced-grant (§0.1/§8).
- **Builds on** [`narf-data-model.md`](./narf-data-model.md): the durable KV is the
  instance state crossing handler boundaries; it is a separate store from
  `ArcContext.vars` (§3); `atom:` survives only as a handle.
- **Applies** [`narf-capability-library.md`](./narf-capability-library.md) §0.1 (box
  edge) to subscription *discovery* (§6) and the manifest review step (§4).
- **Governed by** [`narf-effects-and-safety.md`](./narf-effects-and-safety.md): the
  trust model (§0.1) and the parked-enforcement disposition (§5/§6/§13) are its §0/§1
  applied to the reactive tier. Determinism/correlation/effects enforcement all stay
  parked there, not built here.
- **Refines** [`harness-daemon-boundary.md`](./harness-daemon-boundary.md): §9's
  durable-side placement (ref/promise store, atom-invocation tree, traces) is the
  instance store here. The upward daemon→named-handler bridge (§2/§11) is a **new**
  primitive the boundary topology *requires*, not one it already defines.
- **Subsumes** the routing-packet *dispatcher* role and the node-graph *runtime*
  role; **retains** `bro_orchestrate_run` for static-artifact state machines and the
  packet AST only as an optional invisible predicate fast-path.
- **Hub:** [`bro-harness.md`](./bro-harness.md);
  `system-defaults/memories/workflow-orchestration.md` and
  `system-defaults/memories/atoms.md` document the current behavior this converges —
  read them before treating §12 as already true.

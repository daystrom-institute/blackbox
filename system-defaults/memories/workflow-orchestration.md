+++
title = "Workflow orchestration — mermaid-shaped arcs dispatched by the daemon"
tags = ["workflow", "workflows", "orchestrate", "orchestration", "arc", "arcs", "mermaid", "state-diagram", "runbook", "protocol", "state-machine", "phase", "phases", "multi-phase", "pipeline", "sequencer", "choreography", "process", "loop", "crucible", "overmind", "gate", "gates", "choice", "fork", "late-inject", "late_inject", "fire-and-forget", "subworkflow", "compose", "composition", "arc-thread", "compaction-anchor", "policy-packet", "advisor-as-packet", "branch", "branching", "retry", "retry-loop", "back-edge", "back-edges", "dispatch-workflow", "bro-orchestrate", "bro_orchestrate"]
order = 24
template = false
+++
# Workflow orchestration — JSON arcs dispatched by the daemon

The workflow engine lets you describe a protocol between actors — executor, ensemble, advisor, user — as a JSON spec where each node carries a typed `next` transition (`goto` / `branch` / `fork` / `terminal`), and the daemon walks the transitions deterministically. The LLM stops cosplaying a state machine; the daemon owns the loop.

**If your task looks like this, consider a workflow:**
- A multi-phase arc where the next step depends on a previous step's outcome.
- A protocol you'd otherwise implement as 200 lines of prose "Advisor rules" / "Implementer protocol" / "Discipline invariants."
- Anything with retry loops, convergence checks, async steering, or structured branching on a verdict.
- Anything you've had to carry as an LLM-driven state machine across many turns.

**If it doesn't, don't reach here:**
- One-shot questions → just `bro_exec` / `bro_resume`.
- Exploratory / free-form conversations — the graph constraint is dead weight.
- Work where the structure you'd need genuinely changes mid-flight — workflows aren't great at spontaneous restructuring. (Sub-workflow composition helps, but restructuring itself belongs in the caller.)

## The two tools

- **`bro orchestrate run <workflow.json>`** (CLI) or **`bro_orchestrate_run(workflow=…)`** (MCP tool) — dispatch a workflow, block until it terminates, return the event log + `arc_thread_id`.
- **`bro orchestrate status <thread-id>`** (CLI) or query `bbox_notes(thread_id=<arc_thread_id>)` — read the arc's audit trail + latest compaction anchor.

`--dry-run` (CLI) / `dry_run=true` (MCP) validates + summarizes without dispatching.

## Workflow shape (minimum viable)

```json
{
  "name": "my-arc",
  "version": 1,
  "actors": {
    "worker": { "kind": "executor", "brofile": "<brofile>", "durable": true }
  },
  "start": "A",
  "nodes": {
    "A": { "actor": "worker", "prompt": "first step", "next": { "type": "goto", "to": "B" } },
    "B": { "actor": "worker", "prompt": "use ${A.output}", "next": { "type": "terminal" } }
  }
}
```

Top-level `start` names the entry node; every node carries a `next` clause whose tagged variant decides where control goes after the node returns. The daemon validates: every `next` target must reference a declared node, every named actor must be declared, every late_inject source must exist, the graph must reach at least one `terminal`, and every node must be reachable from `start`.

JSON Schema for the full spec lives at `schema/workflow.schema.json` — point editor tooling at it for in-place validation.

## Actor kinds

- **`executor`** — single bro, dispatched via `bro_exec` / `bro_resume`. `durable: true` means successive nodes referencing the same actor resume the same session (shared `session_id`, shared context).
- **`ensemble`** — team broadcast via `bro_broadcast`; members run concurrently; the node's output is the labeled concatenation of every member's output. Requires a team on the daemon (see `bro_team(action="create", …)`).
- **`advisor`** — functionally an executor with a narrower persona lens. Convention-only split.
- **`user`** — human escalation point. Hitting a user node halts the arc with a `blocked` note carrying the prompt.

**Hook-only / pure-routing nodes** are expressed by leaving `actor` empty (`""`) — the node fires its `on_enter` / `on_exit` hooks, captures the rendered `prompt` as the node output (so `${NodeName.output}` references stay legal), and follows `next` like any other node. Use this for `Setup` / `Done` patterns and for nodes whose only work is HTTP / shell / var-twiddling hooks.

## Transition primitives

Every node's `next` is a tagged enum:

- `{ "type": "goto", "to": "<node>" }` — single forward edge OR back-edge for cycles. The same shape covers both; cycles are just goto's that land before the current node.
- `{ "type": "branch", "cases": { "<verdict>": "<node>", … }, "default": "<node>" }` — multi-way branch keyed by the most recent gate verdict (default selector). `cases` keys SHOULD match the node's `gate` packet's classification lattice; verdicts not enumerated fall through to `default` if set, else halt with a clear error listing available cases.
- `{ "type": "fork", "branches": ["<node>", …], "continue_to": "<node>" }` — dispatch each `branches` entry fire-and-forget, then advance the main walk to `continue_to`. To wait for the branches before running a downstream node, set that node's `wait_for: ["<branch>", …]` field — the engine joins each listed in-flight source at node entry.
- `{ "type": "terminal" }` — successful arc termination.

## Node fields

- `actor` — actor name; empty string = hook-only node.
- `prompt` — template; `${NodeName.output}` substitutes a prior (or late-joined) node's output. Also supports `${vars.X}`, `${outputs.Node.field}`, `${meta.X}`, `${last_signal.payload.X}`, `${env.X}`.
- `gate` — optional packet id applied to the node's output. The packet's classification becomes the verdict consumed by the node's `branch` transition (or any downstream branch).
- `gate_mode` — `first` (default) returns the first matching rule's classification; `all` evaluates every rule, aggregates findings, returns the lattice-highest-priority classification.
- `mode` — `sync` (default) or `fire_and_forget` (dispatch, advance, let late_inject join later).
- `late_inject.from` — node id; at this node's entry the engine waits for the source's in-flight task, captures its output, and makes it available for `${source.output}` substitution. Enables the optimistic-review pattern.
- `wait_for` — array of node ids whose in-flight tasks must complete before this node's body runs. Used for fork → fan-in (the join semantic). Empty/None = no fan-in.
- `retry.max_generations` — visit-count ceiling. Each re-entry (including back-edges) bumps; exceeding halts. Retried prompts auto-prepend `[retry — attempt N, prior gate verdict: X]`.
- `subworkflow` — full inline workflow spec. Node runs the sub-spec to completion; sub-workflow's node_outputs are concatenated (labeled `sub:<node>`) as this node's output. Sub-arcs open their own `bbox_thread`. Depth-limited to 5.
- `subworkflow_ref` — name of a workflow installed in the daemon's registry. Mutually exclusive with `subworkflow`. Resolved at dispatch time.
- `imports` / `exports` / `import_renames` — only meaningful with `subworkflow*`; thread parent vars in, promote child vars back out.
- `wait` — signal-driven suspension. Mutually exclusive with `actor` and `subworkflow*`.

## Workflow-level fields

- `start` — entry node id (required).
- `policy_packet` — optional packet id applied to the arc's own state at each node boundary. Entity is `{ step, just_ran, next, completed, completed_count, in_flight, in_flight_count, last_verdict, visit_counts }`. Classifications are arc-level verdicts: `halt` stops the arc, `escalate` writes a `blocked` note, `warn` writes a `surprise` note, anything else is no-op. This is "advisor as packet" — deterministic arc-health without an LLM in the decision loop.
- `vars_schema` — optional kind/required schema for arc vars; `set_var`, `default_var`, and `inc_var` hooks validate against this.
- `on_arc_exit` / `on_arc_cancel` — hook arrays fired at terminal (any outcome) / cancellation respectively.

## Standard shapes

### Linear durable chain (e2e-smoke pattern)

```
A → B → terminal
```

Durable executor carries context from A to B via shared session. Simplest non-trivial workflow.

### Gate + branch (e2e-gated pattern)

```
Decide ─▶ branch[yes→Say_Yes, no→Say_No]
                  Say_Yes → terminal
                  Say_No  → terminal
```

`Decide` has a `gate` packet; its classification becomes the last verdict. The node's `next.branch.cases` routes by matching verdict to case key. Unpicked cases never dispatch.

### Back-edge retry (blind-convergence pattern)

```
Propose → Review ─▶ branch[revise→Propose, converged→Work] → terminal
```

Node with a branch whose `revise` case targets the proposal step. `retry.max_generations` caps the loop so you can't infinite-retry.

### Fork + fire-and-forget + late_inject (e2e-async-review pattern)

```
P1 ─▶ fork[branches=[Review], continue_to=P2]
              Review (fire_and_forget) → terminal
              P2 (late_inject.from=Review) → terminal
```

The fork dispatches Review fire-and-forget and advances the main walk to P2 immediately. P2 declares `late_inject.from: Review` so at P2's entry the engine joins Review's output (with timeout) and makes it available as `${Review.output}`. The optimistic async-steering pattern — review runs in parallel with work, lands on the next turn boundary.

### Fork + explicit fan-in (e2e-fork-join pattern)

```
Setup ─▶ fork[branches=[Left, Right], continue_to=Summarize]
                 Left  (fire_and_forget) → terminal
                 Right (fire_and_forget) → terminal
                 Summarize (wait_for=[Left, Right]) → terminal
```

Both `Left` and `Right` run in parallel; `Summarize` declares `wait_for: ["Left", "Right"]` so its body runs only after both branches complete and their outputs are joined into `node_outputs`.

### Sub-workflow composition (e2e-composition pattern)

```
Preamble → SubGreet (subworkflow=…) → Closing → terminal
```

Where `SubGreet` has `subworkflow: <full inline spec>`. Sub-arc runs as a unit, opens its own thread, concatenated sub-outputs flow back as the parent node's output.

### Policy-as-packet (e2e-policy pattern)

Workflow declares a top-level `policy_packet`. At every boundary, engine builds an arc-state entity and applies the packet. Halts, escalates, or warns based on classification — runaway-visit detectors, time ceilings, arc-shape invariants.

## Gate packets — how classifications become branches

A gate packet is a rule-packet (see `sm-rule-packets`) whose classifications drive the same-node branch transition. Lattice keys SHOULD match `cases` keys; verdicts not enumerated fall through to `default` if set, else halt with a clear error listing the available cases.

For a convergence check, e.g. `["converged", "revise"]`:
- Rules fire on the node's output (entity shape: `{output: <string>, node: <node_id>}`)
- Rule classification becomes the verdict
- The same node's `next.branch.cases` selects the target by matching verdict to case key

If no rule matches, verdict is None; the branch transition halts with "no prior gate verdict." Add a fallback rule (`emit: "fallback"` with `True` antecedent) to guarantee a default, OR set `default` on the branch to specify the fallthrough target without packet-side fallback.

## Arc thread — every run is persistent and queryable

Every `bro orchestrate run` auto-opens a `bbox_thread(kind=work_item, name="wf-<workflow-name>")`. Structured notes trail every major event:

- `done` — each node completes
- `learned` — every gate verdict + every rolling compaction anchor (`ANCHOR [step N, …]`)
- `surprise` — late-joins, policy warnings, fork fan-ins
- `blocked` — user pauses, policy halts, policy escalations

The arc is reconstructable from the thread alone. Observers (you, other sessions, `bbox_inbox`) can read the whole arc via `bbox_notes(thread_id=<id>)` or `bro orchestrate status <id>`. The latest `ANCHOR` note is the compaction summary — what's completed, what's in-flight, last verdict, visit counts.

## Authoring workflow — minimum sane loop

1. Start from a catalog example in `examples/workflows/` that matches your interaction shape.
2. Substitute your own actors (brofiles, teams).
3. Write per-node prompts with `${PriorNode.output}` substitutions where later nodes depend on earlier ones.
4. Wire transitions: every node ends in a `next` clause. Forward edges are `goto`; cycles are also `goto` (just point back). Use `branch` only when a `gate` packet's verdict picks among targets.
5. If branching: compile a gate packet whose classifications match the branch `cases` keys. `bbox_compile(domain=…, classification_lattice=[…], rules=[…])` then plug the returned packet id into the node's `gate` field.
6. Cross-validate without dispatching: `bro orchestrate run <spec> --dry-run`. Fix any reported mismatch before a real run.
7. Dispatch. Read `bro orchestrate status <arc_thread_id>` mid-flight or post-hoc.

## Common traps

- **Gate verdict lattice should match branch case keys.** If packet lattice is `["approved", "revise"]`, the consuming branch's `cases` keys should be `approved` and `revise`. Engine rejects unmatched verdicts at runtime with a clear error listing available cases (or routes to `default` if set).
- **Gate packet with no catchall rule can produce a null verdict.** If every rule antecedent fires false, the gate emits None. The downstream branch then halts with "no prior gate verdict." Fix: add a fallback rule (`emit: "fallback"` with `True` antecedent) OR set `default` on the branch.
- **`branch` and `default` are different failure absorbers.** A null verdict (no rule fired) halts unless the branch has `default`. An unrecognized verdict (rule fired with a value not in `cases`) also halts unless `default` is set. Both produce clear errors naming the failure mode.
- **Fork branches are always fire-and-forget.** The main walk advances to `continue_to` immediately after dispatch. To wait for branches before a downstream node, use that node's `wait_for` field.
- **Retry ceiling is per-visit-count.** Back-edge goto's bump the visit count. `retry.max_generations: 3` means a node can be visited at most 3 times.
- **Sub-workflow recursion is depth-limited** at 5 by default.
- **Durable actors persist within one arc, not across runs.** Fresh `bro orchestrate run` starts fresh sessions.
- **Empty `actor` is legal** for hook-only / pure-routing nodes; the validator only complains when a non-empty actor name fails to resolve.

## Debugging arcs at runtime

Trigger install tools expose the actual spec and nested selector schemas.
Cron specs accept `tz="UTC"` (default) or `tz="Local"`, case-insensitively;
unsupported zones are rejected before new installation or artifact activation.
Local means the daemon's system timezone. Legacy stored unsupported zones
retain a warned UTC fallback on restart. Cron `concurrency=0` lifts the cap and
allows every tick to dispatch; it does not disable the cron.

`bro_arc_status` returns bounded summaries by default; omit `arc_id` to list
arcs and continue with `offset=next_offset`. Use an `arcId` or its distinct
`arc_thread_id` to select one arc. If waits or correlations are omitted, read
`detail="full"` before diagnosing signal matching. `bro_arc_result` returns
small selected results inline and explicitly previews large selections; `keys`
selects vars and `include_node_outputs=true` requests node prose. Its default
vars omit the duplicate `_structured_exit`, which is exposed as `structuredExit`.
For either tool, `detail="full"` returns exact selected JSON in `body.text`:
continue with `cursor=body.next_cursor` using the same selectors, concatenate
all pages, then parse JSON. Changed evidence rejects the cursor; restart.

When an arc parks unexpectedly or a webhook seems not to route, the canonical loop walks the chain backward from the arc to the inlet:

1. **`bro_arc_status(arc_id=…)`** — confirm the arc is parked, see which node, see the registered wait correlations (typed `Number(24)` vs string `"24"` is a classic mismatch).
2. **`bro_signals(signal=<name>)`** — did the signal the arc is waiting on actually arrive?
   - `outcome=matched` → wait resolved (if arc still didn't advance, look at the gate that follows).
   - `outcome=no_matching_wait` → signal arrived but its correlation didn't match any pending wait. The captured `idle_pending` snapshot shows what waits had the same signal name — the diff between that and the signal's correlation IS the bug.
3. **`bro_webhook_deliveries(name=<webhook>)`** — if no signal arrived at all, walk back one step. Did the webhook arrive? What did the routing packet classify it as? `verdict_classification` of `ignore` / `no_match` for an event you expected to route reveals a missing or mis-shaped routing rule. `extracted_entity` shows what the extractor projected — useful when the routing rule isn't matching because the event's actual value differs from what the rule expects (e.g. Forgejo sends `action: "synchronized"` not `"synchronize"`).
4. **`bro_webhook_replay(name, body, headers)`** — once you suspect a routing-rule fix, replay a synthetic payload through the same path the live webhook would take. See the verdict, iterate without needing the upstream to fire a real event.
5. **`bbox_notes(thread_id=<arc>)`** — the arc's audit trail (done / learned / surprise / blocked) plus rolling `ANCHOR` compaction summaries.

Control:

- **`bro_arc_cancel(arc_id)`** — manually stop a runaway / mis-dispatched / no-longer-relevant arc. The runner observes between node iterations and inside Wait suspensions, exits with status `cancelled`, runs `on_arc_cancel` (if declared) followed by `on_arc_exit`. Cleanup hooks (worktree teardown, etc.) fire automatically.
- **`cancel_arc` routing verdict** — emit from a routing packet to cancel arcs by correlation tuple (e.g. an upstream "PR closed without merge" event cancelling the arc that was waiting on its merge).

## When NOT to use a workflow

- The problem genuinely needs free-form dialogue — a graph constraint fights you.
- The task is one dispatch — `bro_exec` is lighter.
- Structure changes mid-run in ways you can't express via goto back-edges — the workflow becomes a struggle. Restructure upstream of the engine.
- You'd be compiling a gate packet for a single verdict you could just have the LLM return directly — keep LLMs doing LLM work.

## See also

- `sm-rule-packets` — how to compile the gate + policy packets workflows depend on
- `sm-bro-dispatch-patterns` — the primitives workflows are built on
- `sm-scoped-pins` — complementary short-horizon guidance tool
- [Workflow Examples](../../examples/workflows/workflow-examples.md) — runnable catalog + authoring guide
- `schema/workflow.schema.json` — JSON Schema for editor tooling

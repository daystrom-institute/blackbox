# Workflow orchestration — mermaid-shaped arcs dispatched by the daemon

The workflow engine lets you describe a protocol between actors — executor, ensemble, advisor, user — as an embedded `stateDiagram-v2` plus structured metadata, and the daemon walks the graph deterministically. The LLM stops cosplaying a state machine; the daemon owns the loop.

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
  "nodes": {
    "A": { "actor": "worker", "prompt": "first step" },
    "B": { "actor": "worker", "prompt": "use ${A.output}" }
  },
  "graph": "stateDiagram-v2\n    [*] --> A\n    A --> B\n    B --> [*]"
}
```

Two halves: structured metadata (actors, nodes) and an embedded mermaid graph (as a string). The daemon cross-validates them before any dispatch — every activity node in the graph needs metadata, every metadata entry needs to be reachable.

## Actor kinds

- **`executor`** — single bro, dispatched via `bro_exec` / `bro_resume`. `durable: true` means successive nodes referencing the same actor resume the same session (shared `session_id`, shared context).
- **`ensemble`** — team broadcast via `bro_broadcast`; members run concurrently; the node's output is the labeled concatenation of every member's output. Requires a team on the daemon (see `bro_team(action="create", …)`).
- **`advisor`** — functionally an executor with a narrower persona lens. Convention-only split.
- **`user`** — human escalation point. Hitting a user node halts the arc with a `blocked` note carrying the prompt.

## Graph primitives (mermaid subset)

- `[*] --> A` start
- `A --> [*]` terminal
- `A --> B` sequential
- `A --> B: label` labeled (consumed by choice + fork)
- `state X <<choice>>` — verdict routing node; picks the outgoing edge whose label matches the last gate verdict
- `state X <<fork>>` — first outgoing is the sync continuation; remaining outgoing are fire-and-forget async branches, their handles held for `late_inject` joins
- `state X <<join>>` — declared, not yet executable (use `late_inject` for fan-in today)
- `%%` line comments

Only `<<choice>>` and `<<fork>>` nodes may have multiple outgoing edges. Activity nodes are strictly single-successor.

## Node fields

- `actor` — required unless `subworkflow` is set
- `prompt` — template; `${NodeName.output}` substitutes a prior (or late-joined) node's output
- `gate` — optional packet id applied to the node's output. The packet's classification becomes the verdict for the next choice node.
- `mode` — `sync` (default) or `fire_and_forget` (dispatch, advance, let late_inject join later)
- `late_inject.from` — node id; at this node's entry the engine waits for the source's in-flight task, captures its output, and makes it available for `${source.output}` substitution. Enables the optimistic-review pattern.
- `retry.max_generations` — visit-count ceiling. Each re-entry (including back-edges through choice nodes) bumps; exceeding halts. Retried prompts auto-prepend `[retry — attempt N, prior gate verdict: X]`.
- `subworkflow` — full inline workflow spec. Node runs the sub-spec to completion; sub-workflow's node_outputs are concatenated (labeled `sub:<node>`) as this node's output. Sub-arcs open their own `bbox_thread`. Depth-limited to 5.

## Workflow-level fields

- `policy_packet` — optional packet id applied to the arc's own state at each node boundary. Entity is `{ step, just_ran, next, completed, completed_count, in_flight, in_flight_count, last_verdict, visit_counts }`. Classifications are arc-level verdicts: `halt` stops the arc, `escalate` writes a `blocked` note, `warn` writes a `surprise` note, anything else is no-op. This is "advisor as packet" — deterministic arc-health without an LLM in the decision loop.

## Standard shapes

### Linear durable chain (e2e-smoke pattern)

```
[*] → A → B → [*]
```

Durable executor carries context from A to B via shared session. Simplest non-trivial workflow.

### Gate + choice branch (e2e-gated pattern)

```
[*] → Decide → Decide_Route → Say_Yes / Say_No → [*]
```

`Decide` has a `gate` packet; its classification becomes the last verdict. The `<<choice>>` routes by matching verdict to edge label. Unpicked branches never dispatch.

### Back-edge retry (blind-convergence pattern)

```
[*] → Propose → Review → Converge? → (revise → Propose) OR (converged → Work) → [*]
```

Choice node with a back-edge to the proposal step. `retry.max_generations` caps the loop so you can't infinite-retry.

### Fork + fire-and-forget + late_inject (e2e-async-review pattern)

```
[*] → P1 → fork → P2 → [*]
            ↘ Review (async)
Review → P2: late-inject
```

Fork's first outgoing is the sync continuation (P2); the other branch (Review) is dispatched fire-and-forget. P2 declares `late_inject.from: Review` so at P2's entry the engine joins Review's output (with timeout) and makes it available as `${Review.output}`. The optimistic async-steering pattern — review runs in parallel with work, lands on the next turn boundary.

### Sub-workflow composition (e2e-composition pattern)

```
[*] → Preamble → SubGreet → Closing → [*]
```

Where `SubGreet` has `subworkflow: <full inline spec>`. Sub-arc runs as a unit, opens its own thread, concatenated sub-outputs flow back as the parent node's output.

### Policy-as-packet (e2e-policy pattern)

Workflow declares a top-level `policy_packet`. At every boundary, engine builds an arc-state entity and applies the packet. Halts, escalates, or warns based on classification — runaway-visit detectors, time ceilings, arc-shape invariants.

## Gate packets — how classifications become branches

A gate packet is a rule-packet (see `sm-rule-packets`) whose classifications are used as edge labels on the following choice node. Lattice must match the edge labels exactly.

For a convergence check, e.g. `["converged", "revise"]`:
- Rules fire on the node's output (entity shape: `{output: <string>, node: <node_id>}`)
- Rule classification becomes the verdict
- `<<choice>>` picks the outgoing edge labeled with that verdict

If no rule matches, verdict is None; a downstream choice node will halt with a clear error listing the available edge labels. Add a fallback rule (`emit: "fallback"` with `True` antecedent) to guarantee a default.

## Arc thread — every run is persistent and queryable

Every `bro orchestrate run` auto-opens a `bbox_thread(kind=work_item, name="wf-<workflow-name>")`. Structured notes trail every major event:

- `done` — each node completes
- `learned` — every gate verdict + every rolling compaction anchor (`ANCHOR [step N, …]`)
- `surprise` — late-joins, policy warnings
- `blocked` — user pauses, policy halts, policy escalations

The arc is reconstructable from the thread alone. Observers (you, other sessions, `bbox_inbox`) can read the whole arc via `bbox_notes(thread_id=<id>)` or `bro orchestrate status <id>`. The latest `ANCHOR` note is the compaction summary — what's completed, what's in-flight, last verdict, visit counts.

## Authoring workflow — minimum sane loop

1. Start from a catalog example in `examples/workflows/` that matches your interaction shape.
2. Substitute your own actors (brofiles, teams).
3. Write per-node prompts with `${PriorNode.output}` substitutions where later nodes depend on earlier ones.
4. If you need branching: compile a gate packet whose classifications match the edge labels on your choice node. `bbox_compile(domain=…, classification_lattice=[…], rules=[…])` then plug the returned packet id into the node's `gate` field.
5. Cross-validate without dispatching: `bro orchestrate run <spec> --dry-run`. Fix any reported mismatch before a real run.
6. Dispatch. Read `bro orchestrate status <arc_thread_id>` mid-flight or post-hoc.

## Common traps

- **Gate verdict lattice must match edge labels.** If packet lattice is `["approved", "revise"]`, choice-node edges must be labeled `approved` and `revise`. Engine rejects unmatched verdicts at runtime with a clear error.
- **Fork node ordering is semantic.** First outgoing edge is the sync continuation (main walk). All others are fire-and-forget. Re-ordering changes meaning.
- **Fan-out outside choice/fork is a spec error.** Activity nodes must have exactly one outgoing edge.
- **Retry ceiling is per-visit-count.** Back-edges through choice nodes count as retries. `retry.max_generations: 3` means a node can be visited at most 3 times.
- **Sub-workflow recursion is depth-limited** at 5 by default.
- **Durable actors persist within one arc, not across runs.** Fresh `bro orchestrate run` starts fresh sessions.
- **The graph string must start with `stateDiagram-v2`.** No other mermaid kinds accepted.

## When NOT to use a workflow

- The problem genuinely needs free-form dialogue — a graph constraint fights you.
- The task is one dispatch — `bro_exec` is lighter.
- Structure changes mid-run in ways you can't express via back-edges — the workflow becomes a struggle. Restructure upstream of the engine.
- You'd be compiling a gate packet for a single verdict you could just have the LLM return directly — keep LLMs doing LLM work.

## See also

- `sm-rule-packets` — how to compile the gate + policy packets workflows depend on
- `sm-bro-dispatch-patterns` — the primitives workflows are built on
- `sm-scoped-pins` — complementary short-horizon guidance tool
- `examples/workflows/README.md` — runnable catalog + authoring guide

# Workflow examples

Workflow specs for `bro orchestrate run`. Each file is a JSON document declaring
`actors`, optional `atom_bindings`, and `nodes`; each node carries a typed `next`
clause (`goto` / `branch` / `fork` / `terminal`) that drives control flow. The
daemon validates every transition target, actor reference, atom binding, and
reachability before any dispatch. JSON Schema at
[`../../schema/workflow.schema.json`](../../schema/workflow.schema.json).

Run any of these:

```bash
bro orchestrate run examples/workflows/<file>.json
```

Point `bro` at fleetd on port 7265 via env or flag:

```bash
BRO_PORT=7265 bro orchestrate run examples/workflows/e2e-smoke.json
bro orchestrate run examples/workflows/e2e-smoke.json --url http://127.0.0.1:7265
```

---

## Catalog

### `e2e-smoke.json` — two-turn durable-actor probe

The minimum viable workflow. Two nodes, one durable executor, prompt substitution between turns. Demonstrates that:

- The daemon loads + validates a spec
- Sequential dispatch works
- `durable: true` makes node 2 a `bro_resume` of node 1's session (same `session_id`)
- `${Greet.output}` interpolates prior output into the next prompt

```
start: Greet
Greet → goto:Riff → terminal
```

Requires a `probe-haiku` brofile on the daemon. Create one with:

```
bro_brofile(action="create", name="probe-haiku", provider="claude",
            model="claude-haiku-4-5-20251001", effort="high", scope="global")
```

### `e2e-async-review.json` — fork + fire-and-forget + late_inject

The optimistic-review pattern with real async execution. `P1` returns → its `next.fork` dispatches `Background_Review` fire-and-forget and continues the main walk to `P2` → `P2` declares `late_inject.from: Background_Review`, so at P2's entry the engine joins the review's output (with timeout) and folds it into P2's prompt via `${Background_Review.output}` substitution.

```
start: P1
P1 → fork{branches=[Background_Review], continue_to=P2}
       Background_Review (fire_and_forget) → terminal
       P2 (late_inject.from=Background_Review) → terminal
```

This is the pattern that used to require an LLM cosplaying a state machine to coordinate. Now it's deterministic: fork → async dispatch → sync continuation → join-at-next-turn-boundary.

### `e2e-fork-join.json` — fork + explicit fan-in via `wait_for`

Fan-out plus synchronous fan-in. `Setup` forks both `LeftBranch` and `RightBranch` fire-and-forget and continues the main walk to `Summarize`; `Summarize` declares `wait_for: ["LeftBranch", "RightBranch"]`, so its body runs only after both branches complete and their outputs are joined.

```
start: Setup
Setup → fork{branches=[LeftBranch, RightBranch], continue_to=Summarize}
          LeftBranch  (fire_and_forget) → terminal
          RightBranch (fire_and_forget) → terminal
          Summarize (wait_for=[LeftBranch, RightBranch]) → terminal
```

### `e2e-ensemble-vote.json` — concurrent ensemble dispatch + aggregation

Ensemble actor validation. A moderator poses a question; an `ensemble` actor backed by a team runs both members concurrently via `bro_broadcast`; member outputs are collected (sorted by member name, labeled `── <member> ──`), merged as the node's output, and flow into a downstream synthesizer via `${PanelVote.output}` substitution. Proves `workflow_dispatch_ensemble` + concurrent `JoinSet` wait + stable output ordering + cross-actor template flow all work end-to-end.

```
start: PoseQuestion
PoseQuestion → goto:PanelVote → goto:Synthesize → terminal
```

Requires a team with at least two brofiles. Default in the spec is `ensemble-duo`. Create one on the daemon via:

```
bro_brofile(action="create", name="probe-haiku-b", provider="claude",
            model="claude-haiku-4-5-20251001", scope="global")
bro_team(action="save_template", name="ensemble-duo",
         members=[{"brofile":"probe-haiku","alias":"haiku-a"},
                  {"brofile":"probe-haiku-b","alias":"haiku-b"}])
bro_team(action="create", name="ensemble-duo", template="ensemble-duo")
```

### `e2e-self-audit.json` — durable-session multi-phase critique

Three-phase self-critique arc with a back-edge: `Summarize` → `IdentifyConcerns` (gated, retry ≤ 3) → branch[revise→IdentifyConcerns, concrete→SanityCheck]. Durable auditor carries context across all turns. The gate packet (`workflow/critique-concreteness`) classifies the critique's structural format — three `CONCERN N` headers with `Issue:` / `Scenario:` / `Fix:` fields — to decide whether the branch advances or back-edges.

```
start: Summarize
Summarize       → goto:IdentifyConcerns
IdentifyConcerns → branch{revise→IdentifyConcerns, concrete→SanityCheck}
SanityCheck     → terminal
```

### `e2e-composition.json` — sub-workflow as a node

A parent workflow embeds a full sub-workflow via a node's `subworkflow` field. The sub-workflow compiles and validates at parent-compile time; at dispatch time it runs as a unit (opening its own arc thread) and its per-node outputs are concatenated (labeled `sub:<node>`) into the parent node's output, available to downstream template substitutions.

```
start: Preamble
Preamble → goto:SubGreet (subworkflow=…) → goto:Closing → terminal
```

`SubGreet` is the composition point — its own actors + nodes + start live inside it. Recursion is depth-limited (5 by default). This is how reusable templates (crucible, ensemble-consensus, etc.) become library-callable rather than pasted-prose.

### `e2e-atom-binding.json` — atom binding as a node

A workflow-local binding maps a short name to a standalone atom ref, then a node
invokes the binding with structured `atom_args`.

```
start: Echo
Echo (atom=echo, atom_ref=atom:echo@v1) → terminal
```

Install the echo atom first:

```
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
```

Then run it:

```
bro orchestrate run examples/workflows/e2e-atom-binding.json
```

Bindings are workflow-local caps: `limits` can tighten the atom contract for
this workflow, but cannot loosen the atom's own effect limits.

### `e2e-policy.json` — workflow-level policy packet (advisor-as-packet)

Attaches a `policy_packet` to the workflow itself. At every node boundary, the engine builds an arc-state entity (step, completed, in-flight, last verdict, visit counts) and applies the packet. The classification drives an arc-level action:

- `halt` — stop the arc (error exit)
- `escalate` — write a `blocked` note on the arc thread, continue
- `warn` — write a `surprise` note, continue
- anything else — no-op

This is the "advisor as packet" move — replacing a continuous LLM advisor round with a deterministic rule evaluator. The arc thread picks up the verdicts as regular notes; `bro orchestrate status` surfaces them in the trail.

Requires a policy packet with an appropriate lattice. Compile one like:

```
bbox_compile(
  domain="workflow/arc-policy",
  classification_lattice=["halt", "escalate", "warn", "continue"],
  prefix_inference={"halt_": "halt", "escalate_": "escalate", "warn_": "warn", "continue_": "continue"},
  rules=[
    {"id": "halt_too_many_steps",
     "antecedent": {"op": "Gt", "field": "step", "value": 20},
     "consequent": "HALT"},
    {"id": "continue_default", "classification": "continue", "emit": "fallback",
     "antecedent": {"op": "True"}, "consequent": "CONTINUE"}
  ]
)
```

### `e2e-gated.json` — gated branch transition

Gate packet + `branch` transition. An activity node produces a verdict via a compiled rule-packet; the same node's `next.branch.cases` routes to whichever target whose key matches the verdict. Unpicked cases never dispatch.

```
start: Decide
Decide → branch{yes→Say_Yes, no→Say_No}
Say_Yes → terminal
Say_No  → terminal
```

Requires a yes/no gate packet. Compile one like so:

```
bbox_compile(
  domain="workflow/yes-no-gate",
  classification_lattice=["yes", "no"],
  prefix_inference={"yes_": "yes", "no_": "no"},
  rules=[
    {"id": "yes_said_yes",
     "antecedent": {"op": "StringContains", "field": "output", "needle": "YES"},
     "consequent": "YES"},
    {"id": "no_said_no",
     "antecedent": {"op": "StringContains", "field": "output", "needle": "NO"},
     "consequent": "NO"}
  ]
)
```

Then plug the returned packet id into the workflow's `"gate"` field.

### `optimistic.json` — async ensemble steering (spec only — needs an `ensemble` team on the daemon)

Ensemble version of the optimistic-review shape in `e2e-async-review.json` — phase 1 completes, fork kicks off a durable ensemble review asynchronously, phase 2 runs, ensemble results late-inject into phase 2's brief when available.

```
start: P1_Executor
P1_Executor → fork{branches=[Ensemble_Durable_Review_1], continue_to=P2_Executor}
                Ensemble_Durable_Review_1 (fire_and_forget) → terminal
                P2_Executor (late_inject.from=Ensemble_Durable_Review_1) → terminal
```

### `blind.json` — converge-then-execute (spec-complete, needs ensemble team)

Rigid blind-review pattern: executor proposes, blind ensemble critiques, iterate until convergence (back-edge), executor implements, fresh blind ensemble reviews the work. Uses a `branch` on the convergence step — same shape as `e2e-gated` but with ensemble critique bodies. Requires a `blind-review-team` team on the daemon.

```
start: Exec_Propose
Exec_Propose         → goto:Ensemble_Blind_Iter
Ensemble_Blind_Iter  → branch{revise→Exec_Propose, converged→Exec_Work}
Exec_Work            → goto:Ensemble_Blind_Final
Ensemble_Blind_Final → terminal
```

---

## Authoring a new workflow

1. **Pick a pattern.** Start from one of the catalog specs that matches the interaction shape you want.
2. **Name your actors or atoms.** Actors declare a dispatch kind (`executor` or `ensemble`), a brofile or team, and whether they're durable. Atom nodes instead reference `atom_bindings` by name and pass `atom_args`. For pure hook-host / routing nodes, leave `actor` empty (`""`).
3. **Set `start`.** Top-level `start` names the entry node.
4. **Write the node metadata.** Every node needs a `next` clause. Activity nodes also need an actor + prompt; `${NodeName.output}` substitutes a prior node's text output. Gates, retry ceilings, late-inject, and `wait_for` declarations are optional.
5. **Compile any gate packets** you referenced. Packet classifications SHOULD match the consuming branch's `cases` keys (e.g., lattice `["yes", "no"]` → cases `{"yes": ..., "no": ...}`). Use `default` on the branch for unmatched-verdict fallthrough.
6. **Dry-run the spec.** `bro orchestrate run <your.json> --dry-run` validates and prints a summary without dispatching.
7. **Dispatch.** `bro orchestrate run <your.json>`. The event log shows every dispatch, gate verdict, branch route, and node completion.

## Common traps

- **Gate verdict lattice should match branch case keys.** If your packet's lattice is `["approved", "revise"]`, your branch's `cases` keys should be `approved` and `revise`. The engine halts at runtime on an unmatched verdict and lists the available cases (or routes to `default` if set).
- **Durable actors persist across nodes *within one arc*, not across arc invocations.** A fresh `bro orchestrate run` starts a fresh set of sessions even if the same actor names appear.
- **Atom bindings require installed atoms.** `bro_orchestrate_run(..., dry_run=true)` performs capability validation before the dry-run summary, so missing atom refs fail early.
- **Fork branches are always fire-and-forget.** The main walk advances to `continue_to` immediately after dispatch. To wait for branches before a downstream node, use that node's `wait_for` field.
- **`late_inject` joins at node entry, with a timeout.** The source node keeps running in the daemon; the target node's dispatch blocks until the source completes (with a 15-minute timeout). If you want zero-wait optimistic, interpose other sync work between the fork and the late-inject target so the source has time to complete.
- **Cycles are just `goto` back-edges.** No special syntax. Retry budgets stay on the node-level `retry` field; `retry.max_generations: 3` means a node can be visited at most 3 times.
- **Sub-workflow recursion is depth-limited.** Default ceiling is 5 levels deep. Exceeding halts the arc.
- **Empty `actor` is legal** for hook-only / pure-routing nodes; the validator only complains when a non-empty actor name fails to resolve.

## When not to use a workflow

Free-form single-turn work doesn't need a workflow — just `bro_exec`. Workflows pay off when:

- The arc spans multiple phases with distinct actors
- You want mechanical gates (packets) deciding advance vs. retry vs. halt
- You need the arc to survive LLM context compaction (the daemon holds state, not any LLM)
- You want inspectable re-runnable protocols — spec-as-code rather than prose-as-protocol

# Workflows

Canonical reference for the blackbox workflow engine - the durable
execution loop that lets you stitch together rule-packet decisions,
hook side-effects, sub-arc composition, and external webhook signals
into a deterministic state machine driven by mermaid graphs.

> **Why a separate doc.** README is for the project. This is for the
> engine. If you're authoring a workflow, this is your map. If you
> just want to *run* an example, head to
> [Keystone Example](../examples/keystone/keystone-example.md).

## Why it exists

An LLM maintaining workflow state across turns drifts. It forgets
phases, re-litigates settled decisions, invents new steps to paper
over mistakes, and dies on context compaction. A daemon-driven loop
doesn't - it has no context to forget. LLMs become stateless function
calls dispatched *into* the loop rather than the loop's substrate.

This is the same lesson Temporal, Restate, Sayiir, and Wolverine
learned for deterministic handlers. The shape adapted here:
**checkpoint-based** (LLMs are non-deterministic so replay is
meaningless), **JSON-spec-driven** (specs are LLM-author-friendly),
**packet-policy-mechanized** (decisions are data, not code),
**MCP-surfaced** (operator commands the engine through the same
protocol agents use).

## TL;DR shape

A workflow is one JSON file:

```jsonc
{
  "name": "issue-to-merged-pr",
  "version": 1,
  "vars_schema": { "issue_number": {"kind": "int", "required": true}, ... },
  "actors":  { "implementer": { "kind": "executor", "brofile": "...", "durable": true } },
  "policy_packet":  "domain:workflow-policy/arc-budget",
  "on_arc_exit":    [ ... cleanup hooks ... ],
  "on_arc_cancel":  [ ... compensating hooks ... ],
  "start": "Setup",
  "nodes": {
    "Setup":     { "actor": "", "on_enter": [ ... ],
                   "next": { "type": "goto", "to": "Implement" } },
    "Implement": { "subworkflow_ref": "implementer-arc",
                   "imports": ["issue_number","owner","repo"],
                   "exports": ["pr_number","branch"],
                   "next": { "type": "goto", "to": "Wait" } },
    "Wait":      { "wait": { "any_of": [{"signal":"pr-merged",
                                          "correlate":{"pr":{"kind":"json_path","path":"vars.pr_number"}}}],
                              "timeout": "24h" },
                   "gate": "domain:workflow-gate/merge-or-review",
                   "next": { "type": "branch",
                             "cases": { "merged": "Done", "ready": "Done" } } },
    "Done":      { "actor": "", "next": { "type": "terminal" } }
  }
}
```

The metadata declares `actors`, optional `atom_bindings`, `nodes`,
`vars_schema`, `policy_packet`, and arc-level hooks. Control flow lives
on each node as a typed `next` clause (`goto` / `branch` / `fork` /
`terminal`); top-level `start` names the entry node. The daemon validates
every transition target, actor reference, atom binding, late_inject source,
and node reachability from `start` before any dispatch fires. The canonical
JSON Schema is at `schema/workflow.schema.json`.

## ArcContext - the universal state container

Every node, hook, gate, policy packet, and webhook routing decision
sees the same JSON-shaped state, called the **ArcContext**:

```text
{
  "vars":        { ... user-set, hook-writable, schema-validated ... },
  "outputs":     { "NodeName": <value>, ... immutable per node ... },
  "meta":        { "arc_id", "workflow_name", "workflow_version",
                   "started_at", "project_dir", "worktree",
                   "arc_outcome", "parent_arc_id", "composition_depth" },
  "last_signal": { "name", "payload", "correlation", "received_at" }?,
  "signal_history": [ ... append-only ... ]
}
```

### Templating

Anywhere a string is interpreted as a template (`prompt`, hook `args`,
correlation values), `${path.into.context}` resolves against this shape:

| Template form                              | Resolves to                              |
|--------------------------------------------|------------------------------------------|
| `${vars.x}`                                | `vars["x"]`                              |
| `${vars.list.0}`                           | `vars["list"][0]` (array indexing)       |
| `${outputs.NodeName}`                      | output of `NodeName` (raw)               |
| `${outputs.NodeName.field.subfield}`       | nested JSON traversal                    |
| `${meta.arc_id}` / `${meta.worktree}`      | arc-intrinsic metadata                   |
| `${last_signal.name}`                      | which Wait signal just resolved          |
| `${last_signal.payload.x}`                 | resolved-signal payload field            |
| `${NodeName.output}` (legacy)              | same as `${outputs.NodeName}`            |

Whole-string templates resolve as their **typed value** (a `${vars.n}`
where `vars.n == 42` becomes the integer 42 in a hook arg). Mixed
strings stringify (`"issue-${vars.n}"` → `"issue-42"`).

Unresolved expressions are left in place verbatim - `${vars.does_not_exist}`
shows up as the literal string in the dispatched prompt rather than
silently becoming the empty string. Misspellings are loud.

### Vars schema

Optional `vars_schema` declares per-key kind and required-ness:

```jsonc
"vars_schema": {
  "issue_number":  { "kind": "int",    "required": true },
  "branch":        { "kind": "string", "required": false },
  "labels":        { "kind": "array" },
  "metadata":      { "kind": "object" }
}
```

Kinds: `bool` / `int` / `float` / `string` / `array` / `object` /
`any`. Hook writes (`SetVar`, `IncVar`, `MergeVar`, `ParseJson`,
`HttpJson` with `into_var`) are kind-validated against the schema -
mismatches fail at write time, not later. `required: true` keys are
enforced at arc start (initial vars), at sub-arc start (imports), and
at terminal state (exports). Unknown keys are accepted (open schema
by design - explicit `required` fields are how you lock down the
surface).

### Packet entity flatten

When a packet evaluates against the ArcContext (gates, hook `when`,
policy packets, webhook routing), the engine flattens to:

```text
{
  "vars":    { ... },
  "outputs": { ... },
  "meta":    { ... },
  "last_signal": { ... }?,
  "node_output":      "<just-completed node's output as string>",
  "node_output_json": <raw>,
  "node_id":          "<just-completed node id>"
}
```

Predicate paths support dotted resolution - `vars.issue_number`,
`outputs.Plan.branch`, `last_signal.name`. Array elements via numeric
indices: `vars.labels.0`. Quantified predicates work too:
`Exists{path:"vars.labels[*]", pred:Eq{field:"$", value:"bug"}}`.

## Actors

Declared in the workflow's `actors` map and referenced by node specs.
Two kinds.

| Kind       | What it does                                            |
|------------|---------------------------------------------------------|
| `executor` | Single bro dispatched via `bro_exec` / `bro_resume`. Used for every single-bro role the workflow declares - implementer, fixer, triager, planner, facilitator, advisor, aggregator, synthesizer, reviewer, etc. The brofile + prompt carry the persona. |
| `ensemble` | Team broadcast: every member runs the same prompt; output is the labeled concatenation. When the node declares a `board` binding (see §Whiteboards), each member's STRICT-JSON output is parsed into typed board actions (post / annotate / vote) and auto-applied by the engine — no tool call required. |

**Why only two kinds.** Earlier iterations had `advisor`, `planner`,
`triager`, `user` as marker types. They didn't pull engine weight -
mechanically each was identical to `executor`, and the marker was
never enforced. Persona / role / contract is a workflow-author concern
carried by the brofile lens + prompt + on_exit `parse_json`
validation. Engine vocabulary describes *dispatch shape*, not roles.

**Human-in-the-loop without a `user` actor.** When a workflow needs
human input - operator approval, external Claude weighing in,
ntfy-routed acknowledgement - the pattern is: open a whiteboard,
register the human as an `operator` role, suspend the arc on a
`board-transitioned` signal correlated to `(board_id, target_phase)`.
The human (or their Claude session, or a slack/ntfy adapter) calls
`whiteboard_post` / `whiteboard_vote` / `whiteboard_transition`
through the same MCP surface in-workflow specialists use. No special
"user" type. See §Whiteboards.

For hook-only / pure-routing nodes (Setup / Done patterns where the
work is on_enter / on_exit hooks and there's no LLM dispatch), leave
the node's `actor` field empty (`""`). The validator only complains
when a non-empty actor name fails to resolve. Hook-only nodes still
fire hooks, capture their rendered `prompt` as the node output (so
`${NodeName.output}` references stay legal), and follow `next`.

Per-actor fields:

- `brofile` - single-bro target (executor / advisor)
- `team` - ensemble target
- `durable: true` - reuse the same session across every node that
  invokes this actor; the engine threads the session id through
  `bro_resume` automatically. Critical for the implementer that has
  to address feedback after initial PR.
- `compaction_anchor: true` - the actor contributes a rolling summary
  to the arc's work_item thread at every boundary, so an orchestrator
  swap-out doesn't lose strategic memory.
- `requires: [Capability, ...]` - see Capability tags below.
- `timeout` - dispatch-wait timeout applied to every node this actor
  runs (each ensemble member individually). Duration string (`"30m"`,
  `"1h"`) or number of seconds; a node's own `timeout` overrides it;
  default 900s. Size it up for long-thinking members - high-effort
  evidence work routinely runs 10-20+ minutes (gap-0301dc75).

Ensemble prompts support per-member identity templating: the literal
`${member.name}` survives ArcContext rendering (unknown heads are left
verbatim) and is substituted with each team member's name at dispatch,
so one node prompt can say `agent_name: "${member.name}"` instead of
duplicating N brofile lenses. Name team members meaningfully (the
admin team upsert accepts `{name, brofile}` member objects alongside
bare brofile strings) - the member name is also the attribution
fallback for whiteboard `board` auto-apply.

## Atom Bindings

A workflow uses `atom_bindings` when a node should call a reusable capability
instead of dispatching an actor directly.

The binding name is local. The atom ref is the contract. That split matters:
workflows can cap a powerful atom for one arc without changing the atom itself.

```jsonc
"atom_bindings": {
  "echo": {
    "atom_ref": "atom:echo@v1",
    "limits": { "dispatches_runs": 0 }
  }
},
"nodes": {
  "Echo": {
    "atom": "echo",
    "atom_args": { "message": "hello" },
    "next": { "type": "terminal" }
  }
}
```

Use this when the workflow should say "run the test-island extractor" rather
than know which brofile, workflow, runner, or adapter currently implements it.

Binding fields:

| Field | Meaning |
|---|---|
| `id` | Optional self-identifying id. If present, it must match the map key. |
| `atom_ref` | Required atom ref: `atom:name@vN` or `atom:name@latest`. |
| `durable` | Reuse the binding's previous invocation id on repeated visits. Profile-backed atoms resume; non-resumable handles are re-invoked. |
| `requires` | Provider capabilities required when the atom resolves to a profile-backed brofile. |
| `limits` | Optional tighter `writes_files`, `dispatches_runs`, `max_depth`, `uses_network` bounds. |
| `supervision_override` / `trace_override` / `portal` | Reserved policy metadata carried on the binding. |

Validation happens before dispatch. The compiler checks binding shape and node
references. Capability validation resolves installed atoms, rejects missing or
inactive refs, verifies binding limits do not exceed the atom's declared effects,
and checks profile-backed provider capabilities.

At runtime the engine calls `atom_invoke`, records the invocation id, reads
`atom_status`, and stores that status JSON as the node output. Deterministic and
adapter atoms usually finish immediately. Profile and workflow atoms may still be
backed by a running bro task or child arc, so downstream nodes should branch on
the trace fields they actually require.

Atom nodes are mutually exclusive with `actor`, `subworkflow*`, `wait`,
`foreach`, and `matrix`, and cannot use `mode: "fire_and_forget"` in v1.
If `atom_args` is absent and `prompt` is present, the rendered prompt is passed
as `{ "prompt": "..." }`; otherwise args default to `{}`.

## Capability tags

Mirrors daystrom's `CapabilityTag` shape. Hard-fail at compile rather
than silent route-around.

| Capability         | Provider availability                                |
|--------------------|------------------------------------------------------|
| `structured_output`| Claude (extension), Codex (`--output-schema`); NOT Gemini CLI |
| `vision`           | Claude, Gemini                                        |
| `long_context`     | Claude (1M); promote to model-keyed when smaller variants matter |
| `tool_use`         | All except Vibe                                       |
| `resume`           | All except Vibe and Gemini                            |

Declare on the actor:

```jsonc
"actors": {
  "structured_review": {
    "kind": "ensemble",
    "team": "review-team",
    "requires": ["structured_output"]
  }
}
```

`bro_orchestrate_run` (and the `bro_workflow_install` + webhook-routing
`start_arc` paths) walk every actor's brofile / team-member brofiles,
resolve to provider, check capability coverage. First missing
capability is a compile error with the offending actor + provider
named. Sub-workflows are recursively validated.

## Nodes

A node declares the unit of work. Fields:

| Field             | Meaning                                                 |
|-------------------|---------------------------------------------------------|
| `actor`           | Which actor runs this node (mutually exclusive with `atom`, `subworkflow*`, and `wait`). |
| `atom`            | Which workflow-local atom binding runs this node (mutually exclusive with `actor`, `subworkflow*`, `wait`, `foreach`, and `matrix`). |
| `atom_args`       | Structured atom input arguments, resolved through ArcContext templating before `atom_invoke`. Whole-string templates preserve JSON type. |
| `prompt`          | Template (rendered against ArcContext).                 |
| `gate`            | Packet id; verdict drives next-node selection at choice nodes. |
| `gate_mode`       | `first` (one verdict, default) or `all` (multi-finding aggregate). |
| `mode`            | `sync` (default) or `fire_and_forget`.                  |
| `retry`           | `{max_generations: N}` - visit-count ceiling.           |
| `timeout`         | Dispatch-wait timeout for this node's actor task (per ensemble member; also the join wait for fire-and-forget sources). Duration string (`"30m"`) or seconds. Overrides the actor's `timeout`; default 900s. |
| `board`           | Whiteboard binding (template → board id, e.g. `"${vars.board_id}"`). Ensemble nodes only. Each member's STRICT-JSON output — one object or an array of `{action: post\|annotate\|vote\|none, …}` items, code fences tolerated — is parsed and applied to the board engine-side with the same phase/role checks as the `whiteboard_*` tools. Attribution: item `agent_name`, else member name. Failures log `board_autoapply_*` events; the node never fails from auto-apply. |
| `late_inject`     | `{from: NodeId, policy: "resume_on_return"}` - fold an async source into this node at its entry. |
| `subworkflow`     | Inline `Workflow` spec (mutually exclusive with `subworkflow_ref`). |
| `subworkflow_ref` | Workflow id resolved from the registry at dispatch time. |
| `imports`         | Vars to copy from parent into the sub's fresh ArcContext (subworkflow nodes only). |
| `exports`         | Vars to promote back to parent on sub completion (missing exports = runtime error). |
| `import_renames`  | `{ local_name: "parent.path.expression" }` - Extractor-style projection from parent context into sub. |
| `on_enter`        | Hook ops fired BEFORE actor dispatch / sub descent / Wait registration. |
| `on_exit`         | Hook ops fired AFTER body returns, BEFORE the gate evaluates. Lets `ParseJson` normalize output the gate sees. |
| `wait`            | `WaitSpec` - suspend the arc on a signal. Mutually exclusive with `actor` + `subworkflow*`. |

## Hooks and the op catalog

Hooks are guarded side effects that fire at lifecycle boundaries:

| Lifecycle slot         | When fires                                              |
|------------------------|---------------------------------------------------------|
| `node.on_enter`        | Before actor dispatch / Wait registration / subworkflow descent. |
| `node.on_exit`         | After actor body returns, BEFORE the gate.              |
| `workflow.on_arc_exit` | At terminal state (success, fail, cancel, timeout).     |
| `workflow.on_arc_cancel` | ONLY when arc is cancelled. Runs BEFORE on_arc_exit.  |

Each hook is one declaration:

```jsonc
{
  "op": "set_var",
  "args": { "key": "branch", "value": "fix/issue-${vars.issue_number}" },
  "when": "domain:workflow-cleanup/keep-on-fail",   // optional packet gate
  "on_failure": "halt",                              // halt | warn | ignore
  "into_var": "result_key"                           // for ops that produce a value
}
```

### Hook gating

A hook's `when` references a packet. The packet evaluates against the
flattened ArcContext. The hook fires only if the packet's verdict is in
the operator-blessed allow set: `allow`, `pass`, `proceed`, `fire`,
`ok`, `delete`, `keep`, `yes`, `true`. Unknown verdicts (e.g. `flag`,
`manual`) do NOT permit firing - conservative on purpose.

### Op catalog (current)

| Op                    | Purpose                                                |
|-----------------------|--------------------------------------------------------|
| `set_var`             | Write `vars[key] = value` (schema-validated).          |
| `default_var`         | Write `vars[key] = value` only when the var is missing or null. |
| `inc_var`             | Increment integer var (default delta 1).               |
| `append_var`          | Append to array var.                                   |
| `merge_var`           | Merge object into object var.                          |
| `parse_json`          | Parse a string-shaped value into JSON; strips ```` ```json ```` fences. |
| `shell`               | Shell command; `cwd` from `meta.worktree`. Two-layer argv[0] allowlist: workflow-level `shell_allowlist` (rides ArcMeta, enforced on every Shell op) intersected with per-op `args.allowlist` (narrows, never widens). Byte-exact matching; bare entries match bare argv[0] only, path-bearing argv[0] needs an exact-path entry. Absent lists preserve trusted-actor behavior. |
| `worktree_create`     | `git worktree add` with smart branch reuse (see below). |
| `worktree_remove`     | `git worktree remove`; idempotent on missing path. No `rm -rf` fallback - surfaces git's error if the path is not a git-tracked worktree. |
| `set_meta`            | Update `meta.worktree` directly (only mutable meta field today). |
| `http_json`           | Generic HTTP request → response into_var. Workflow author composes URL/headers/body using `${env.X}` + `${vars.X}` templates to express any code-host integration without baking platform-specific ops into the engine. `response_kind`: `json` (default - parse, error on non-JSON), `text` (capture body as-is, e.g. for `.diff` URLs), or `auto` (try JSON, fall back to text). `expect_status`: optional override of the default 2xx success window. Transient statuses (429/502/503/504) and transport errors retry with capped exponential backoff honoring Retry-After, for idempotent methods by default; POST/PATCH retry only on explicit opt-in via the `retry` block. |
| `find_first`          | Find the first element of an array variable whose nested fields all equal the targets in `where`, write into_var. Writes `Value::Null` (not Err) when no match - downstream `IsNull` / `IsNonNull` packets branch cleanly. Composable primitive for "find existing PR for this branch" / "find label by name" without baking platform-specific search ops or relying on upstream API filters that may be broken. `where` keys use dotted paths (e.g. `head.ref`). |
| `mcp_call`            | Outbound MCP tool call - sibling of `http_json` but speaking MCP JSON-RPC. Resolves `args.server` against the existing `bro_mcp` registry (global `~/.bro/mcp.json` + project overlay), opens a transient client (stdio child-process or streamable HTTP), invokes `args.tool` with `args.arguments`, captures the result into `vars[into_var]`. Tool errors (`is_error: true`) become op failures so `on_failure` fires. Result normalization preserves typing when `structured_content` is present, falls back to JSON-parsed text content otherwise. Args: `{server, tool, arguments?, timeout_secs?}` (default 300s). Use for engine-level grounding calls - `sast_run` / `sast_findings` against biofilter, `bbox_thread` / `bbox_note` against blackbox-self - instead of dispatching a bro just to make a tool call. |

### `worktree_create` semantics

Three real cases the op handles:

1. **Branch absent** → `git worktree add -b <branch> <path> <base>` (fresh).
2. **Branch present, no worktree references it** → `git worktree add <path> <branch>` (REUSE - common after a prior arc died with `cleanup-policy=keep-on-fail`).
3. **Branch present AND another worktree has it checked out** → fail loudly with the occupant path. Suggests including `${meta.arc_id}` in the branch name for concurrent-arc-safe naming.

### Failure semantics

`on_failure` per hook decides what an op error means:

- `halt` (default) - abort the arc with the op's error.
- `warn` - log + write a `surprise` note on the arc thread, continue.
- `ignore` - log only, continue.

Hooks cannot change which next node runs - that authority stays with
the gate packet. Hooks are guarded *side effects*, not control flow.
If you need conditional branching, that's a node + gate.

## Wait nodes

`WaitSpec` suspends the arc until one of `any_of` signals arrives or
the timeout fires.

```jsonc
"AwaitFeedbackOrMerge": {
  "wait": {
    "any_of": [
      { "signal": "pr-feedback", "correlate": { "pr": "${vars.pr_number}" } },
      { "signal": "pr-merged",   "correlate": { "pr": "${vars.pr_number}" } }
    ],
    "timeout": "7d"
  },
  "gate": "domain:workflow-gate/loop-or-exit"
}
```

### Correlation

Each `WaitSignal.correlate` is a tuple of key → Extractor-Selector
expressions evaluated against the **current** ArcContext at Wait
registration. The canonical form (sorted JSON keys) is the second
component of the `(signal_name, correlation) → (arc_id, wait_id)`
index in the daemon's `WaitStore`. When a signal arrives, the router
walks pending waits whose `correlate` is a subset-match against the
signal's payload-correlation map. **Empty correlation = broadcast**:
matches anything that signals the same name.

### Resolution

When a matching signal arrives:

1. The router pops the first-matching wait from the store.
2. Writes a `SignalRef { name, payload, correlation, received_at }`
   into the wait's resolved-slot.
3. Wakes the arc via the wait's `Notify`.
4. Sibling waits in the same `any_of` are removed from the store
   (race-loser cleanup - only the first signal wins).

The arc resumes, populates `ctx.last_signal` + appends to
`signal_history`, and proceeds to the gate (which can branch on
`last_signal.name`, `last_signal.payload.*`, etc.).

### Timeout

If `timeout` fires before any signal:

1. All sibling waits are removed.
2. A synthetic signal is recorded:
   `SignalRef { name: "__timeout__", payload: { expired: ["sig1", "sig2"] } }`.
3. Gate evaluates as normal (route via `Eq{field: "last_signal.name", value: "__timeout__"}`).

`timeout` accepts `30s`, `5m`, `1h`, `7d` - any decimal followed by a
unit suffix. Absent timeout = wait indefinitely (suitable for
never-firing-but-cancellable arcs).

### Waiting on background bro tasks

Every dispatched task's terminal state emits a hub system event
(`task.completed` / `task.failed` / `task.cancelled`) whose correlation
carries `{task_id}`, and the daemon's system-event signal bridge
resolves matching workflow waits automatically. That makes
"fan a bro out, keep walking, join later" a three-node recipe with no
polling:

```jsonc
"SpawnBro": {
  "on_enter": [
    { "op": "mcp_call",
      "args": { "server": "blackbox", "tool": "bro_exec",
                "arguments": { "bro": "some-brofile", "prompt": "…" } },
      "into_var": "exec_result" },
    { "op": "set_var",
      "args": { "key": "bg_task_id", "value": "${vars.exec_result.taskId}" } }
  ],
  "next": { "type": "goto", "to": "AwaitTask" }
},
"AwaitTask": {
  "wait": {
    "any_of": [
      { "signal": "task.completed",
        "correlate": { "task_id": { "kind": "json_path", "path": "vars.bg_task_id" } } },
      { "signal": "task.failed",
        "correlate": { "task_id": { "kind": "json_path", "path": "vars.bg_task_id" } } }
    ],
    "timeout": "1h"
  },
  "next": { "type": "goto", "to": "Collect" }
}
```

Route on `last_signal.name` after the wait to branch success/failure.
Prefer engine-native `mode: "fire_and_forget"` + `wait_for` when the
work is an actor of THIS workflow; the signal recipe is for tasks the
arc doesn't own - bros dispatched via `bro_exec`, another arc's
workers, externally-started tasks - anything correlatable by task id.

Two ways to compose a subworkflow into a node:

### Inline

```jsonc
"Implement": {
  "subworkflow": { "name": "...", "version": 1, "actors": { ... }, ... },
  "imports": ["issue_number"],
  "exports": ["pr_number"]
}
```

The full sub-spec is embedded. Compiled at parent compile time
(errors surface immediately).

### By id

```jsonc
"Implement": {
  "subworkflow_ref": "implementer-arc",
  "imports": ["issue_number"],
  "exports": ["pr_number"]
}
```

Resolved at **dispatch** time against the daemon's workflow registry.
Lets the same sub be referenced multiple times (e.g. keystone calls
`implementer-arc` once for initial impl, calls a peer
`implementer-feedback-arc` later for revisions) without inlining the
spec twice. Install the referenced workflow with `bro_workflow_install`
or POST `/admin/workflow/install`.

### Imports / exports / renames

The sub gets its own fresh ArcContext. `imports` copies parent vars
in by name. `import_renames` is for the cases where the parent's
shape doesn't match the sub's expected names:

```jsonc
"RunInner": {
  "subworkflow_ref": "issue-to-merged-pr",
  "imports": ["repo", "owner"],
  "import_renames": { "issue_number": "next_issue.0.number" }
}
```

Right-hand side is an Extractor path (dotted). Evaluated against the
parent's ArcContext flatten.

`exports` declares which sub vars get promoted back into the parent
on sub completion. **Missing exports are a runtime error** - if you
declare `exports: ["pr_number"]` and the sub never sets `vars.pr_number`,
the parent fails. This is the contract.

`meta.parent_arc_id` propagates so audit walks back up the call tree.
Composition depth is capped at `MAX_COMPOSITION_DEPTH = 5`; exceeding
errors out at the offending nest.

## Gates and branch transitions

A gate packet runs after the node body, against the flattened
ArcContext (with `node_output` exposed at the top level for legacy
rules). The classification becomes the verdict; the same node's
`next.branch.cases` selects the target whose key matches.

```jsonc
{
  "Decide": {
    "actor": "worker",
    "prompt": "...",
    "gate": "packet-decide",
    "next": {
      "type": "branch",
      "cases": { "yes": "Yes", "no": "No" },
      "default": "No"      // optional fallback for unmatched verdicts
    }
  },
  "Yes": { "actor": "worker", "next": { "type": "terminal" } },
  "No":  { "actor": "worker", "next": { "type": "terminal" } }
}
```

Two modes:

- `gate_mode: "first"` (default) - first-matching rule wins, single verdict.
- `gate_mode: "all"` - every matching rule fires, aggregate verdict is
  the lattice-highest-priority classification across all findings.
  Findings are surfaced as a `learned` note on the arc thread and
  exposed via `bro orchestrate status`.

Back-edges in the graph become natural retry loops. Each visit bumps
`visit_counts[node]`; exceeding `retry.max_generations` halts the
arc. Retried prompts get `[retry - attempt N, prior gate verdict: X]`
prepended automatically.

## Workflow-level policy packets

`policy_packet: <id>` runs at every node boundary against the
flattened ArcContext (with arc-shape additions: `step`, `just_ran`,
`next`, `completed`, `in_flight`, `visit_counts`). Classifications
are arc-level verdicts:

- `halt` - stop the arc immediately (error exit, reason posted to thread).
- `escalate` - write a `blocked` note, continue.
- `warn` - write a `surprise` note, continue.
- anything else - no-op.

This is the mechanization of the advisor loop: instead of dispatching
an LLM at every boundary to read the checkpoint and judge, compile
those rules into a packet once and let them evaluate deterministically.
Useful for runaway-visit detectors, time / step ceilings, arc-shape
invariants.

## Domain-shaped packet refs

Anywhere a workflow names a packet (`gate`, `policy_packet`,
`hook.when`, `webhook.routing_packet`), accept either:

- `packet-XXXXXXXX` - pinned exact compile.
- `domain:<name>` - resolves to the most-recently-compiled packet
  matching `<name>`.

Domain refs let workflow + webhook specs survive packet recompiles
without re-edit. Audit events name the resolved `packet-id` so you
always know which compile actually fired.

## Inlets - webhook, poller, cron

Three inlet primitives, all converging on the same dispatch pipeline:

```text
<inlet emits a flat entity>
  → extractor.project(payload)       [JSON → flat entity]
  → routing_packet.apply(entity)     [verdict]
  → routing::resolve_entity_template [substitute ${entity.X} with typed values]
  → RoutingVerdict::parse            [typed verdict]
  → dispatch_verdict(...)            [start_arc | signal_arc | cancel_arc | ignore | dead_letter]
```

The shared dispatch path is `crate::dispatch_routed_event`. Routing
rules don't know which inlet fed them - they see a flat entity and a
routing-packet id, the rest is the engine's problem. Pick the inlet
that matches your trigger source:

| Inlet     | Trigger source            | Spec lives in                | When to use |
|-----------|---------------------------|------------------------------|-------------|
| Webhook   | External HTTP POST + signature | `webhooks/<name>.json`  | Upstream pushes to you (Forgejo / GitHub / Stripe / generic JSON). Lower latency than polling, but needs ingress. |
| Poller    | Scheduled HTTP fetch (data rides on the tick) | `pollers/<name>.json` | Upstream is poll-only OR you have no public ingress. Carries an `HttpFetchSpec` + optional `iterate` for array responses + dedup ring. |
| Cron      | Calendar / clock (no fetch - entity is operator-supplied `payload`) | `crons/<name>.json` | Trigger is time-based, not event-based. Nightly maintenance, hourly sweeps, scheduled SAST squashing. The dispatched arc does its own data acquisition (typically via `mcp_call` hooks). Concurrency cap (default 1) skips ticks while a prior arc is still in flight. |

### Webhook

Webhooks are operator-installed (signature scheme + Extractor +
routing packet). The pipeline:

```text
HTTP POST /webhook/<name>
  → verify_signature(secret)         [HmacSha256 (configurable header
                                       + optional `prefix=`) | None
                                       (loopback-bind only)]
  → idempotency dedup PEEK           [per-webhook delivery-id ring, 8192 ids]
  → extractor.project(payload)       [JSON → flat entity]
  → routing_packet.apply(entity)     [verdict]
  → dispatch_verdict(...)            [start_arc | signal_arc | cancel_arc | ignore | dead_letter]
  → dedup COMMIT on success          [a failed dispatch leaves the id fresh, so a
                                      durable sender's retry reprocesses instead of
                                      bouncing off duplicate_dropped-as-200]
```

### Extractor

Tiny AST that projects a webhook payload (plus `_headers` map for
header-driven routing - operator names the header in their extractor;
the engine doesn't know the sender) into a flat entity:

```jsonc
"extractor": {
  "outputs": {
    "event":  { "kind": "json_path", "path": "$._headers.x-gitea-event" },
    "action": { "kind": "json_path", "path": "$.action" },
    "issue_number": {
      "kind": "default",
      "inner": { "kind": "json_path", "path": "$.issue.number" },
      "fallback": null
    },
    "owner":  { "kind": "json_path", "path": "$.repository.owner.login" },
    "repo":   { "kind": "json_path", "path": "$.repository.name" }
  }
}
```

Selector kinds: `json_path`, `const`, `default { inner, fallback }`,
`concat { parts: [...] }`, `coalesce { sources: [...] }` (first
non-null wins; lets one extractor field cover divergent payload
shapes - e.g. Forgejo's `pull_request_review` puts the comment text
at `.review.body` while `pull_request_review_comment` uses
`.comment.body`). Deliberately small - no transformations (regex,
case folding, math). Those belong in packet predicates or downstream
nodes.

### Routing verdicts

The routing packet is a normal rule packet whose `consequent` is
either a JSON-encoded string (since `Rule.consequent` is the typed
`packets::Value` enum, structured shapes go through string-encoding):

```jsonc
{
  "id": "start_arc_on_issue_opened",
  "classification": "start_arc",
  "antecedent": { "op": "All", "args": [
    { "op": "Eq", "field": "event",  "value": "issues" },
    { "op": "Eq", "field": "action", "value": "opened" }
  ]},
  "consequent": "{\"route\":\"start_arc\",\"workflow\":\"issue-to-merged-pr\",\"initial_vars\":{}}"
}
```

Or a string shorthand for `ignore` / `dead_letter`.

Verdict shapes:

```jsonc
{ "route": "start_arc",   "workflow": "<id>", "initial_vars": { ... } }
{ "route": "signal_arc",  "signal": "pr-merged", "correlate": { "pr": 42 } }
{ "route": "cancel_arc",  "correlate": { "pr": 42 } }   // not yet implemented
{ "route": "ignore" }
{ "route": "dead_letter", "reason": "..." }
```

### `start_arc` semantics

When a webhook resolves to `start_arc`, the engine:

1. Resolves the workflow by id from the registry.
2. Compiles + capability-validates.
3. Builds `merged_vars`: extracted entity (excluding `_headers` and
   `null` values) overlaid by the verdict's explicit `initial_vars`
   (last-writer wins).
4. Resolves `project_dir`:
   - `${WEBHOOK_NAME_UPPER}_PROJECT_DIR` env override
   - `WebhookSpec.default_project_dir`
   - `None` (worktree hooks fail loudly - better than silent fallback to cwd).
5. Spawns the arc in a background task. Webhook returns immediately
   with `{status: "arc_started", workflow: <id>}`.

### Default-deny

No-match on the routing packet is **dead-lettered**, not ignored.
The extracted entity is included in the response for forensics.

### Replay

POST `/webhook/<name>/replay` with the same payload runs extractor →
routing packet → verdict **without dispatching**. Use to debug routing
rules without firing arcs.

### Poller

Scheduled HTTP-source inlet - operationally "a webhook whose source is
a tokio interval pulling from a URL instead of an inbound POST." Spec
shape:

```jsonc
{
  "name": "forgejo-open-issues",
  "every_seconds": 120,
  "source": {
    "method": "GET",
    "url": "${env.FORGEJO_BASE_URL}/api/v1/repos/owner/repo/issues?state=open",
    "headers": { "Accept": "application/json" }
  },
  "iterate":      { "kind": "json_path", "path": "$" },
  "extractor":    { "outputs": { ... } },
  "dedup_id_path": { "kind": "json_path", "path": "$.id" },
  "routing_packet": "domain:webhook-routing/forgejo",
  "default_project_dir": "/path/to/local/clone"
}
```

- `every_seconds` is clamped above `BBOX_POLLER_MIN_INTERVAL_SECS`
  (default 5s) - operators can't accidentally hammer an upstream.
- `source` is the same `HttpFetchSpec` shape the workflow `http_json`
  op consumes. `${env.X}` is resolved at spec-load time (no per-tick
  ArcContext to resolve `${vars.X}` against - credentials live in env).
- `iterate` (optional) - array path; each element becomes its own
  event (extractor runs per-element). Absent → whole response is one
  event.
- `dedup_id_path` (optional) - stable id per item; in-memory recent-
  seen ring (1024 cap, per-poller, resets on daemon restart). Repeated
  items are dropped before dispatch.

Manage with `bro_poller_install` / `bro_poller_list` MCP tools, or the
plain-HTTP `/admin/poller/install` endpoint. Persisted under
`store_dir/pollers/<name>.json`; restored on daemon startup.

### Cron

Calendar-driven inlet - sibling of webhook + poller. Distinction from
poller: poller fetches HTTP per tick (data rides on the tick), cron
carries no fetch (entity is operator-supplied `payload` plus synthetic
`cron_name` + `tick_at` fields). Use cron when the trigger is time-
based and the dispatched arc itself does the data acquisition.

The `bro_cron_install`, `bro_poller_install`, and `bro_webhook_install` schemas
describe their actual spec objects and nested selectors, extractors, signature
schemes, HTTP fetch options, and retry policies. Supply `spec` as an object;
legacy stringified JSON specs remain accepted by the runtime parser.

```jsonc
{
  "name": "sastquatch-daily",
  "schedule": "0 0 9 * * *",       // 6-field: sec min hour dom mon dow
  "payload": {
    "owner": "sastquatch-admin",
    "repo": "quat"
  },
  "concurrency": 1,                 // 0 lifts the cap; ticks still dispatch
  "routing_packet": "domain:cron-routing/sastquatch",
  "default_project_dir": "/path/to/local/clone"
}
```

- `schedule` uses the `cron` crate's 6- or 7-field form (seconds-first;
  prefix a classic 5-field cron with `0 ` to run-at-second-0). Validated
  at install time. `bro_cron_upcoming` is a pure helper that returns
  the next N scheduled times for a candidate expression - use to sanity-
  check before installing.
- `payload` is operator-supplied entity fields. Synthetic `cron_name`
  and `tick_at` (RFC3339 UTC) are merged in at tick time so routing
  rules can discriminate without operator boilerplate. Operator-supplied
  keys win on collision.
- `concurrency` caps in-flight arcs spawned by this cron. Default 1
  (skip ticks while a prior arc is still running - the most common
  case for daily sweeps). Set 0 to lift the cap. The counter
  decrements when the dispatched arc terminates; failed dispatches
  refund immediately.
- `tz` accepts `UTC` (default) or `Local`, ASCII case-insensitively. Local
  schedules use the daemon's system timezone with DST-aware evaluation.
  Unsupported names, including IANA timezone names, are rejected before MCP
  installation, HTTP administration, or artifact activation can persist the
  spec or start a timer. Existing stored specs with unsupported names retain
  their warned UTC fallback on restart; replace those specs with an explicit
  supported zone to remove that ambiguity.
- The routing-packet entity sees `{cron_name, tick_at, ...payload}`.
  Most cron arcs route to `start_arc` unconditionally, but you can
  branch on `cron_name` if multiple crons share a routing packet.

Manage with `bro_cron_install` / `bro_cron_list` / `bro_cron_upcoming`
MCP tools, or the plain-HTTP `/admin/cron/install` endpoint. Persisted
under `store_dir/crons/<name>.json`; restored on daemon startup.

### Inlet selection cheat sheet

- **Event arrives at known time, low frequency, you control the source** → webhook.
- **Event arrives at known time but you can't push (closed network, polling-only API)** → poller.
- **Trigger is time-based, no upstream event to react to** → cron.
- **Resilience layering** → webhook + poller against the same upstream, accept the duplicate-dispatch cost (workflow-level idempotency catches it).
- **Event-driven AND time-bounded** (e.g. "open a PR but auto-close after 7 days") → webhook for the open, cron sweeping for the close.

## Whiteboards

Multi-agent deliberation surface, first-class in the engine. Posts
(proposals / claims / concerns / informational), annotations
(challenge / corroborate / resolve / validation), and votes accumulate
on a board, advanced through phases (blind → read → validate →
debate → resolve → archived) by a facilitator-or-operator role.

Where webhooks and crons are *inlet* primitives (events arriving
into the engine), whiteboards are a *deliberation* primitive - a
shared structured log that any audience can read and write through
the same MCP surface:

- **In-workflow ensemble specialists.** Their dispatched bro
  prompts instruct them to call `whiteboard_post` (and later
  `whiteboard_annotate` / `whiteboard_vote`) from inside their turn.
  Each member's brofile lens carries its `agent_name` so the bro
  knows which name to use without prompt-side per-member
  interpolation.
- **In-workflow facilitators (single bro, executor mechanics).**
  Drive phase transitions via `whiteboard_transition`. Phase
  transitions emit a `board-transitioned` signal correlated to
  `(board, target_phase)` through the shared
  `dispatch_routed_event` pipeline - same machinery webhooks use.
- **External agents - operator's Claude session, dispatched help,
  eventually humans through slack / ntfy adapters.** Read board
  state via `whiteboard_state`, act via the same write tools. The
  board IS the human-in-the-loop surface; no separate escalation
  registry, no `user` actor type.

### Spec shape

Whiteboards aren't operator-installed like webhooks/pollers/crons -
they're created on demand by workflows (via `mcp_call → whiteboard_open`)
or by external clients (via `whiteboard_open` MCP tool directly). The
board id is the operator's choice; convention is
`<workflow-name>-<arc_id>` or a deliberation-specific slug.

```jsonc
// In a workflow node's on_enter:
{
  "op": "mcp_call",
  "args": {
    "server": "blackbox",
    "tool": "whiteboard_open",
    "arguments": {
      "board_id": "${vars.board_id}",
      "topic": "ADR #${vars.issue_number}: ${vars.issue_title}",
      "project": "${vars.worktree_path}",
      "arc_thread_id": "${meta.arc_thread_id}",
      "opened_by": "facilitator"
    }
  }
}
```

### Phase protocol

| Phase       | Allowed actions                                 |
|-------------|-------------------------------------------------|
| `blind`     | post (specialists post without seeing others)   |
| `read`      | (none - observation phase, advance when ready)  |
| `validate`  | annotate (validation only, with required result)|
| `debate`    | annotate (challenge / corroborate / resolve), vote |
| `resolve`   | (none - frozen, facilitator synthesizes outside)|
| `archived`  | (terminal - board moved to `archive/` directory)|

Skip rule: `read → debate` is legal (skip validate). All other
transitions follow the canonical order.

### Conflict detection

`whiteboard_conflicts` evaluates the board state and returns three
kinds of conflicts:

- **direct_overlap** - two posts target the same `target_file` +
  `target_location`
- **cascade_collision** - post A's `cascade_targets` includes post
  B's direct target
- **severity_disagreement** - two posts share a `finding_ref` but
  disagree on `severity`

This generalizes phaser's domain-specific shapes; the operator's
posts decide whether the conflict types apply by setting (or not
setting) the relevant fields.

### Resume on phase transition

A workflow can suspend on a board's phase advance using the existing
`wait` primitive - no new variant needed:

```jsonc
"AwaitResolve": {
  "actor": "",
  "wait": {
    "any_of": [
      {
        "signal": "board-transitioned",
        "correlate": {
          "board": { "kind": "json_path", "path": "vars.board_id" },
          "phase": { "kind": "const", "value": "resolve" }
        }
      }
    ],
    "timeout": "1h"
  },
  "next": { "type": "goto", "to": "Synthesize" }
}
```

The transition tool fires the signal; the wait correlates on
`(board, phase)`; routing-pipeline machinery resolves the wait and
the arc resumes. Same shape webhook ingress uses to resume on PR
events.

### Persistence

Boards are JSON files under `$store_dir/whiteboards/<id>.json`,
atomically written via tempfile + rename, restored on daemon
startup. Archived boards move to `$store_dir/whiteboards/archive/`
and are not loaded back into memory - they exist for audit only.

### Where to dispatch what

| Surface | Who calls | Why |
|---------|-----------|-----|
| `whiteboard_open` from `on_enter` hook (mcp_call) | Engine, on Setup | Predictable id, deterministic registration |
| `whiteboard_register` from `on_enter` hook (mcp_call) | Engine, on Setup | All specialists registered up-front so role checks work |
| `whiteboard_post` / `whiteboard_annotate` / `whiteboard_vote` from inside dispatched bro | Specialist (LLM) | The structured deliberation IS the agent's output |
| `whiteboard_transition` from `on_enter` hook (mcp_call) | Engine, on phase-advance node | Deterministic - facilitator's "decide to advance" is part of arc shape, not dispatched |
| `whiteboard_state` / `whiteboard_summarize` | Anyone (specialist mid-turn, facilitator at synthesis, external Claude joining) | Read-only inspection |
| `whiteboard_archive` from `on_enter` of Done | Engine, terminal | Strip live state after the artifact ships |

### Comparison with phaser

The engine's whiteboard machinery absorbs phaser's mk0 protocol
(blind → read → debate → resolve, posts / annotations / votes,
conflict detection, auto-transition advisory). Phaser stays peer
software in the daystrom-claude-plugins monorepo; bridgecrew /
isolinear / ARS keep using the stdio MCP until they migrate. The
engine version is a same-shape superset accessible in-process.

## Operator-blessed registries

Three registries, all persisted to disk and restored on daemon
startup. Workflow specs reference packets by domain and webhooks by
name; specs themselves never inline secrets or routing logic.

| Registry  | On-disk path                                         | Install MCP tool          | Plain-HTTP admin endpoint        |
|-----------|------------------------------------------------------|---------------------------|----------------------------------|
| Workflows | `${BRO_HOME}/workflows/<id>.json`                    | `bro_workflow_install`    | `POST /admin/workflow/install`   |
| Webhooks  | `${BRO_HOME}/webhooks/<name>.json`                   | `bro_webhook_install`     | `POST /admin/webhook/install`    |
| Packets   | `${PACKETS_PATH}/<scope>/<packet-id>.json`           | `bbox_compile` (MCP)      | `POST /admin/packet/compile`     |

Brofiles + teams have their own MCP surface (`bro_brofile`, `bro_team`)
plus admin shortcuts (`/admin/brofile/upsert`, `/admin/team/upsert`)
for install scripts that can't easily speak rmcp's streamable-HTTP
transport.

The admin endpoints are loopback-only by default (`BBOX_BIND=127.0.0.1`).
Set `BBOX_BIND=0.0.0.0` if you need a Docker container on the same
host (Forgejo/Gitea webhook → `http://172.17.0.1:7264/webhook/...`).
**Closed-network assumption applies in 0.0.0.0 mode.** The dev daemon
template ships at `0.0.0.0`; prod stays at loopback.

## Audit and observability

`bro_arc_status` defaults to an arc-id-sorted summary list (10 arcs, maximum 20).
Continue list pages with `offset=next_offset`, or select one arc by `arc_id` using
either its `arcId` or distinct `arc_thread_id`. Unknown ids fail explicitly.
Summary rows keep current position, verdict, admission key, and completed-node
count; full node history and visit-count maps are omitted. Pending waits are
scoped to the selected arcs, with an exact count and up to ten previews. Small
wait correlations retain their JSON types. A large correlation is explicitly
omitted, requiring full detail before diagnosing signal matching.

`bro_arc_result` accepts an arc id or workflow task id for task-backed arcs.
It selects final vars with `keys` and includes node prose only when
`include_node_outputs=true`. Default vars omit `_structured_exit` because the
same value is returned as `structuredExit`; an explicit request for that var
keeps it. Small selected results remain inline. Large selected results return
`preview=true`, `omittedFields`, and bounded key previews, never a partial value
disguised as the complete result.

Both tools accept `detail="full"` for exact selected JSON in `body.text` pages.
Continue with `cursor=body.next_cursor` on the same tool and selectors,
concatenate all text, then parse JSON. `body_limit` requests 4 to 4096 UTF-8 bytes;
JSON escaping can reduce page size. Cursors reject changed selected evidence,
so restart without a cursor if a live arc advances. Status list `limit` and
`offset` apply only to summary lists, not selected arcs or full-detail reads.
These projections do not change the workflow engine's internal state or results.


Every `bro orchestrate run` opens a workflow-origin
`bbox_thread(kind=work_item)`. The returned `arc_thread_id` is the audit handle.
Normal `bbox_thread_list` calls hide workflow-origin threads by default; pass
`include_workflows=true` to inspect arc scaffolding explicitly.

For direct task supervision without workflow state, use [Bro Runtime](bro-runtime.md).
For inbox and note hygiene, use [Knowledge Store](knowledge-store.md).

| Surface                                  | What it shows                                                           |
|------------------------------------------|-------------------------------------------------------------------------|
| `bro orchestrate peek [<thread-id>]`     | Live in-flight state from the `running_arcs` registry. No id = all.    |
| `bro orchestrate status <thread-id>`     | Note trail + latest compaction anchor for one arc.                     |
| `bro orchestrate list [--limit N]`       | Recent arcs catalog with final status + anchor.                        |
| `bbox_notes(thread_id=<arc>)`            | Every structured note the engine wrote (kind: done / learned / surprise / blocked). |
| `bbox_inbox`                              | Arcs currently flagged for attention (failed, blocked, etc.).          |
| `bro_arc_status(arc_id=<id>)`            | Snapshot + pending-wait registrations for one arc.                     |
| `bro_signals(signal=, since=, outcome=)` | Recent signal-dispatch events as a bounded ring buffer. Each entry: `(timestamp, signal, correlation, outcome, matched_arc_id, idle_pending)`. On `outcome=no_matching_wait` the `idle_pending` snapshot shows what waits were registered with the same signal name but didn't match - the diff between what arrived and what was waiting is one read away. |
| `bro_webhook_deliveries(name=, since=, verdict_classification=)` | Recent webhook deliveries (live + replay). Each entry: extracted entity, routing verdict classification, response. Filter by name to focus on one inlet. |
| `bro_webhook_replay(name, body, headers)` | Replay a synthetic payload through an installed webhook's extractor + routing packet WITHOUT dispatching. Returns the extracted entity + verdict consequent (after `${entity.X}` substitution). Recorded into the same delivery buffer with `source: replay`. |
| `bro_arc_cancel(arc_id=<id>)`            | Trip an arc's cancellation token. Runner observes between node iterations and inside Wait suspensions, exits with status `cancelled`, runs `on_arc_cancel` + `on_arc_exit`. |
| HTTP `GET /tail` (SSE)                   | Live event stream from the daemon - every node_dispatch / hook_ok / wait_registered / gate_applied / ... |

Compaction anchors: at every node boundary the engine writes a
rolling `ANCHOR [step N, just-ran='X', next='Y']: completed=[...]
in_flight=[...] verdict=Z visits=[...]` note. Lets observers
reconstruct arc state without reading every event.

## Lifecycle

```text
spawn arc
  → seed initial_vars (schema-validated)
  → open arc thread (bbox_thread kind=work_item, origin=workflow)
  → walk graph from [*]
      → for each activity node:
          on_enter hooks (gated)
          dispatch (actor / wait / subworkflow)
          on_exit hooks (gated)        ← runs BEFORE gate
          gate packet (if any)         ← chooses next-node label
      → policy_packet at every boundary (halt/escalate/warn/continue)
  → terminal state (success / failed / cancelled / timeout)
      → set meta.arc_outcome
      → on_arc_cancel hooks (only if cancelled)
      → on_arc_exit hooks (always)
  → return WorkflowRunResult { status, vars, outputs, events, arc_thread_id }
```

## Currently implemented

- ArcContext (vars / typed outputs / meta / last_signal / history) with full templating including `${env.X}` for credentials
- Routing verdicts can carry typed correlation tuples + payload via
  `${entity.X}` template substitution (resolved against the
  extracted entity before verdict parse) - same `${X}` shape the
  workflow templater uses, applied to a different scope
- Three convergent event inlets: webhook (signed inbound POST), poller
  (scheduled HTTP fetch - data rides on the tick), cron (calendar-
  driven, no fetch - entity is operator-supplied `payload` plus
  synthetic `cron_name` + `tick_at`). All three extract → route →
  dispatch through one shared `dispatch_routed_event` so a workflow
  doesn't care how it was triggered. Pollers reuse `HttpFetchSpec`
  (same primitive the `http_json` op consumes), explode array
  responses via `iterate`, dedup by operator-named id field, and
  resolve `${env.X}` in source URL/headers per-tick (no secrets
  persisted to disk). Crons use the `cron` crate's 6-field expression
  form, support a `concurrency` cap (default 1, skip ticks while a
  prior arc is in flight; set 0 to lift the cap), and decrement the
  in-flight counter when the dispatched arc terminates.
- Hook lifecycle: on_enter, on_exit, on_arc_exit, on_arc_cancel
- Op catalog: SetVar, IncVar, AppendVar, MergeVar, ParseJson, Shell,
  WorktreeCreate (with smart branch reuse), WorktreeRemove, SetMeta,
  HttpJson (generic HTTP; `response_kind: json|text|auto`),
  FindFirst (array search by dotted-path equality - composable
  primitive for idempotent lookups), McpCall (outbound MCP tool
  call - JSON-RPC over stdio child-process or streamable HTTP;
  resolves server name through the existing `bro_mcp` registry; lets
  hooks inject deterministic tool results without dispatching a bro).
  Engine carries no platform knowledge; workflow author composes
  generic ops + `${env.X}` + `${vars.X}` to express any code-host
  integration.
- Hook gating via `when: domain:...` + `on_failure: halt|warn|ignore`
  (gate-packet errors route through `on_failure` too - no silent skips)
- Terminal hook `halt` failures rewrite `meta.arc_outcome` to `failed:
  ...` so cleanup failures are visible in the arc summary
- Wait nodes with `any_of` race + correlation tuples + timeout
  (synthetic `__timeout__` signal)
- Subworkflows: inline + by-id registry refs + imports/exports/renames;
  capability validation runs at dispatch, not just install
- Capability tags: provider catalog + actor `requires` + workflow-compile validation
- Domain-shaped packet refs (`domain:...`)
- Hook-only nodes (empty `actor` field) for Setup / Done patterns
- Webhook ingress: HmacSha256 (configurable header + optional `prefix=`
  for `sha256=` style senders), None for loopback-only testing,
  idempotency dedup, replay endpoint, default-deny on no-match
- Operator-blessed registries (workflows / webhooks) + admin HTTP
- Actor kinds: just `executor` and `ensemble`. Persona / role /
  contract (advisor, planner, triager, facilitator, specialist,
  reviewer, aggregator, …) is brofile-lens + prompt + on_exit
  `parse_json` validation, never an engine type. Empty `actor: ""`
  for hook-only / pure-routing nodes.
- Whiteboards: in-engine multi-agent deliberation primitive (port of
  phaser's mk0 protocol). 10 `whiteboard_*` MCP tools, three
  audiences (in-workflow ensemble specialists / facilitator,
  external Claude operators, eventual humans through slack/ntfy
  adapters) sharing one surface. Phase transitions emit
  `board-transitioned` signals through the same dispatch_routed_event
  pipeline webhooks use, so workflows resume on transitions via the
  existing `wait` primitive correlated to `(board, target_phase)` -
  no new wait variant needed. Replaces the deprecated `user` actor
  kind: humans-in-the-loop join boards as agents, no special engine
  type for them.
- Transition shapes: `goto` (forward edges + back-edges), `branch`
  (verdict-routed multi-way with optional `default`), `fork`
  (fire-and-forget side-dispatch + `continue_to`) paired with
  `wait_for` on the join node for fan-in, `terminal` (arc end)
- Gate modes: `first` (single verdict), `all` (multi-finding aggregate)
- Workflow-level `policy_packet` (advisor-as-packet)
- `late_inject` for fire-and-forget → join-on-next-turn
- Per-actor `durable: true` session continuity + `compaction_anchor: true`
- Arc threads + structured notes + rolling compaction anchors
- `bro_arc_signal` / `bro_arc_status` MCP tools
- `--dry-run` validates + summarizes without dispatching
- `bro orchestrate run --stream` SSE event firehose during execution

## Arc durability

Top-level arcs are crash-resumable. The engine writes a durable
checkpoint (full workflow spec, ArcContext, node outputs, actor
sessions, visit counts, step budget) under `<store_dir>/arcs/` at every
node boundary and at Wait-node entry, and removes it at terminal state.
On boot, a rehydration pass replays what survived:

- Arcs parked on a `wait` re-enter their wait node (on_enter hooks
  skipped), re-register the same correlations into the in-memory
  `WaitStore`, and replay the system-events ledger for anything that
  arrived while the daemon was down. A worker parked on a signal for
  days survives restarts invisibly.
- Arcs that died mid-node-body (or waiting with live fork dispatches)
  are marked `interrupted`, never silently re-run: node bodies are not
  idempotent. Interrupted arcs surface in `/orchestrate/peek`,
  `bro_arc_status`, and a `blocked` note on the arc thread.
- Direct signals (webhook `signal_arc` verdicts, `bro_arc_signal`)
  that find no matching wait are persisted to the system-events ledger,
  so a wait registered later - including one re-registered by
  rehydration - still consumes them. Arcs track consumed event ids, so
  a wait revisited in a loop never resolves twice off the same event.
- Sub-workflow arcs do not checkpoint independently in v1; a crash
  inside one interrupts the parent arc.

Durability semantics worth knowing:

- Replayed signals are at-least-once ACROSS arcs: ledger catch-up is
  per-arc (each arc tracks its own consumed ids), so two arcs parked on
  the same signal + correlation can both consume one persisted idle
  signal, where a live delivery resolves exactly one wait. One arc per
  external subject is the admission invariant's job, not the ledger's.
- A wait resolution is checkpointed BEFORE on_exit hooks and gates run,
  so a crash in that window rehydrates as a loud `interrupted` arc
  rather than silently double-consuming the signal.
- Finite waits persist an absolute deadline; rehydration opens the
  REMAINING window (timing out immediately when it passed during the
  outage) instead of restarting the clock.
- If a checkpoint write fails, the arc's stale checkpoint is removed
  and a `blocked` note lands on the arc thread: crash-resume is
  disabled for that arc rather than risking a stale-state resurrection.
  Idle direct signals report `durable_persist` in the dispatch response
  so callers can tell "parked in the ledger" from "lost".
- The narrow crash window between a LIVE signal resolving a wait and
  that resolution checkpoint can still lose the signal; everything
  coarser is durable.

## Singleton admission

A workflow can declare an `admission` contract:

```json
"admission": {"key": ["issue_number"], "on_conflict": "signal"}
```

At most one non-terminal top-level arc runs per (workflow,
canonicalized key tuple), where the tuple is drawn from the named
initial vars after the entity / initial_vars merge. The runner claims
the key atomically at arc start - every start path, including
rehydration resume - and releases it at terminal state, so two webhook
deliveries racing for the same issue can spawn two arcs but only one
ever runs; the loser terminates with `duplicate admission` naming the
holder. The webhook StartArc path additionally checks BEFORE spawning
and applies `on_conflict`: `ignore` drops the duplicate (response names
the holder), `signal` converts it into a signal on the running arc
(default name `arc-duplicate-start`, correlation = the admission tuple,
payload = the duplicate's merged vars) - the second mention becomes
steering input instead of a second worker. A missing key var refuses
the start loudly. `bro_arc_status` surfaces the held key. Interrupted
(crash-orphaned) arcs do not hold their key.

## Retention

What an arc leaves behind, and what bounds it:

- Durable checkpoints: removed at terminal state; only interrupted
  arcs keep theirs, and those are operator-visible debts, not silent
  accretion.
- Signal and webhook-delivery logs: bounded in-memory rings (~200),
  gone on restart.
- Arc threads and notes: append-only and durable BY DESIGN - they are
  the audit trail, including for one-off inline-spec runs. Policy:
  ad-hoc topologies are cheap to mint and are not auto-deleted; triage
  accretion through `bbox_inbox` staleness surfacing and resolve dead
  arc threads in bulk via `bbox_thread`. If a deployment mints
  thousands of throwaway arcs, prefer an installed workflow id over
  inline specs so runs share one lineage, and prune with the storage
  maintenance surfaces (`bbox_storage_gc`) rather than hand-deleting
  store files.

## Phase-next

- Generic `bro orchestrate resume <thread-id>` - manual re-entry for
  `interrupted` arcs (rehydration already auto-resumes waiting arcs;
  interrupted ones currently need a fresh run).
- YAML workflow loader (JSON-only today; one-line add once `serde_yaml`
  is introduced).
- Push-storm debouncing (real deployments want a `vars.review_in_progress`
  flag the routing packet checks). Pattern documented; not yet wired
  in the keystone example.

## Examples

- [Whiteboard Example](../examples/whiteboard/whiteboard-example.md) - end-to-end:
  ADR-tagged issue webhook → 3-specialist ensemble posts blind to
  whiteboard → debate (annotate + vote) → facilitator synthesizes ADR
  markdown → PR opens → auto-merge. The reference arc for the
  whiteboard primitive + multi-round durable ensemble.
- [Sastquatch Example](../examples/sastquatch/sastquatch-example.md) - end-to-end:
  cron tick → analyzer (mcp_call → biofilter sast_*; executor picks
  a finding cluster) → fixer subworkflow → wait → ensemble reviewer →
  loop on feedback → auto-merge. The reference arc for cron + mcp_call.
- [Keystone Example](../examples/keystone/keystone-example.md) - end-to-end:
  Forgejo webhook → arc → implementer subworkflow → wait → reviewer
  ensemble → wait-loop until merged → cleanup hooks. Real LLM
  dispatch. Full layout + adaptation guide in the example's README.
- [Workflow Examples](../examples/workflows/workflow-examples.md) - smaller
  shape catalog: linear, gated, ensemble, fork-join, blind-convergence,
  optimistic-review, self-audit.

## Authoring loops

If you don't want to hand-write specs:

- `bro_orchestrate_author(charter, brofile, hint?)` - prose-charter
  to validated spec via an authoring LLM. Auto-retries on compile
  failure with the error appended.
- Always pair with `bro_orchestrate_run(workflow, dry_run=true)` to
  validate before dispatching.

For runtime debugging - the canonical "an arc is stuck, why?" loop:

1. **`bro_arc_status`** - confirm the arc is parked, see which node, see the registered wait correlations.
2. **`bro_signals(signal=<name>)`** - did the signal the arc is waiting on actually arrive? `outcome=matched` means the wait resolved (arc should have advanced; if it didn't, look at the gate). `outcome=no_matching_wait` means the signal arrived but its correlation didn't match any pending wait - `idle_pending` shows what was waiting at dispatch time, the diff between that and the signal's correlation IS the bug (typed `pr: 24` vs string `pr: "24"` is the classic).
3. **`bro_webhook_deliveries(name=<webhook>)`** - if the signal never arrived, walk back one step. Did the webhook actually arrive? What did the routing packet classify it as? `verdict_classification` of `ignore` / `no_match` for an event you expected to route reveals a missing or mis-shaped routing rule. `extracted_entity` shows what the extractor projected - useful when the routing rule isn't matching because the event field's actual value differs from what the rule expects (Forgejo sends `action: synchronized` not `synchronize`, etc.).
4. **`bro_webhook_replay(name, body, headers)`** - once you suspect a routing-rule fix, replay a synthetic payload through the same path the live webhook would take, see the verdict, iterate without needing the upstream to fire a real event.
5. **`bbox_notes(thread_id=<arc>)`** - the arc's audit trail with structured notes (done / learned / surprise / blocked) and rolling `ANCHOR` compaction summaries.

Control:

- `bro_arc_cancel(arc_id)` - manually stop a runaway / mis-dispatched / no-longer-relevant arc. Cleanup hooks fire automatically.
- `cancel_arc` routing verdict - emit from a routing packet to cancel arcs by correlation tuple (e.g. an upstream "PR closed without merge" event cancelling the arc that was waiting on its merge).

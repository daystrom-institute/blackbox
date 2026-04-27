# Workflows

Canonical reference for the blackbox workflow engine — the durable
execution loop that lets you stitch together rule-packet decisions,
hook side-effects, sub-arc composition, and external webhook signals
into a deterministic state machine driven by mermaid graphs.

> **Why a separate doc.** README is for the project. This is for the
> engine. If you're authoring a workflow, this is your map. If you
> just want to *run* an example, head to
> [`examples/keystone/`](examples/keystone/README.md).

## Why it exists

An LLM maintaining workflow state across turns drifts. It forgets
phases, re-litigates settled decisions, invents new steps to paper
over mistakes, and dies on context compaction. A daemon-driven loop
doesn't — it has no context to forget. LLMs become stateless function
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

The metadata declares `actors`, `nodes`, `vars_schema`, `policy_packet`,
and arc-level hooks. Control flow lives on each node as a typed `next`
clause (`goto` / `branch` / `fork` / `terminal`); top-level `start`
names the entry node. The daemon validates every transition target,
every actor reference, every late_inject source, and every node's
reachability from `start` before any dispatch fires. The canonical
JSON Schema is at `schema/workflow.schema.json`.

## ArcContext — the universal state container

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

Unresolved expressions are left in place verbatim — `${vars.does_not_exist}`
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
`HttpJson` with `into_var`) are kind-validated against the schema —
mismatches fail at write time, not later. `required: true` keys are
enforced at arc start (initial vars), at sub-arc start (imports), and
at terminal state (exports). Unknown keys are accepted (open schema
by design — explicit `required` fields are how you lock down the
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

Predicate paths support dotted resolution — `vars.issue_number`,
`outputs.Plan.branch`, `last_signal.name`. Array elements via numeric
indices: `vars.labels.0`. Quantified predicates work too:
`Exists{path:"vars.labels[*]", pred:Eq{field:"$", value:"bug"}}`.

## Actors

Declared in the workflow's `actors` map and referenced by node specs.

| Kind       | What it does                                            |
|------------|---------------------------------------------------------|
| `executor` | Single bro dispatched via `bro_exec` / `bro_resume`.    |
| `ensemble` | Team broadcast: every member runs the same prompt; output is the labeled concatenation. |
| `advisor`  | Like executor; convention is narrower tool surface / persona lens. |
| `user`     | Halts the arc with a `blocked` note carrying the prompt. Resume = re-dispatch with whatever resolves the pause. |

For hook-only / pure-routing nodes (Setup / Done patterns where the
work is on_enter / on_exit hooks and there's no LLM dispatch), leave
the node's `actor` field empty (`""`). The validator only complains
when a non-empty actor name fails to resolve. Hook-only nodes still
fire hooks, capture their rendered `prompt` as the node output (so
`${NodeName.output}` references stay legal), and follow `next`.

Per-actor fields:

- `brofile` — single-bro target (executor / advisor)
- `team` — ensemble target
- `durable: true` — reuse the same session across every node that
  invokes this actor; the engine threads the session id through
  `bro_resume` automatically. Critical for the implementer that has
  to address feedback after initial PR.
- `compaction_anchor: true` — the actor contributes a rolling summary
  to the arc's work_item thread at every boundary, so an orchestrator
  swap-out doesn't lose strategic memory.
- `requires: [Capability, ...]` — see Capability tags below.

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
| `actor`           | Which actor runs this node (mutually exclusive with `subworkflow*` and `wait`). |
| `prompt`          | Template (rendered against ArcContext).                 |
| `gate`            | Packet id; verdict drives next-node selection at choice nodes. |
| `gate_mode`       | `first` (one verdict, default) or `all` (multi-finding aggregate). |
| `mode`            | `sync` (default) or `fire_and_forget`.                  |
| `retry`           | `{max_generations: N}` — visit-count ceiling.           |
| `late_inject`     | `{from: NodeId, policy: "resume_on_return"}` — fold an async source into this node at its entry. |
| `subworkflow`     | Inline `Workflow` spec (mutually exclusive with `subworkflow_ref`). |
| `subworkflow_ref` | Workflow id resolved from the registry at dispatch time. |
| `imports`         | Vars to copy from parent into the sub's fresh ArcContext (subworkflow nodes only). |
| `exports`         | Vars to promote back to parent on sub completion (missing exports = runtime error). |
| `import_renames`  | `{ local_name: "parent.path.expression" }` — Extractor-style projection from parent context into sub. |
| `on_enter`        | Hook ops fired BEFORE actor dispatch / sub descent / Wait registration. |
| `on_exit`         | Hook ops fired AFTER body returns, BEFORE the gate evaluates. Lets `ParseJson` normalize output the gate sees. |
| `wait`            | `WaitSpec` — suspend the arc on a signal. Mutually exclusive with `actor` + `subworkflow*`. |

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
`manual`) do NOT permit firing — conservative on purpose.

### Op catalog (current)

| Op                    | Purpose                                                |
|-----------------------|--------------------------------------------------------|
| `set_var`             | Write `vars[key] = value` (schema-validated).          |
| `inc_var`             | Increment integer var (default delta 1).               |
| `append_var`          | Append to array var.                                   |
| `merge_var`           | Merge object into object var.                          |
| `parse_json`          | Parse a string-shaped value into JSON; strips ```` ```json ```` fences. |
| `shell`               | Sandboxed shell command (gated via packet policy in v-next; today reads `cwd` from `meta.worktree`). |
| `worktree_create`     | `git worktree add` with smart branch reuse (see below). |
| `worktree_remove`     | `git worktree remove`; idempotent on missing path. No `rm -rf` fallback — surfaces git's error if the path is not a git-tracked worktree. |
| `set_meta`            | Update `meta.worktree` directly (only mutable meta field today). |
| `http_json`           | Generic HTTP request → response into_var. Workflow author composes URL/headers/body using `${env.X}` + `${vars.X}` templates to express any code-host integration without baking platform-specific ops into the engine. `response_kind`: `json` (default — parse, error on non-JSON), `text` (capture body as-is, e.g. for `.diff` URLs), or `auto` (try JSON, fall back to text). `expect_status`: optional override of the default 2xx success window. |
| `find_first`          | Find the first element of an array variable whose nested fields all equal the targets in `where`, write into_var. Writes `Value::Null` (not Err) when no match — downstream `IsNull` / `IsNonNull` packets branch cleanly. Composable primitive for "find existing PR for this branch" / "find label by name" without baking platform-specific search ops or relying on upstream API filters that may be broken. `where` keys use dotted paths (e.g. `head.ref`). |

### `worktree_create` semantics

Three real cases the op handles:

1. **Branch absent** → `git worktree add -b <branch> <path> <base>` (fresh).
2. **Branch present, no worktree references it** → `git worktree add <path> <branch>` (REUSE — common after a prior arc died with `cleanup-policy=keep-on-fail`).
3. **Branch present AND another worktree has it checked out** → fail loudly with the occupant path. Suggests including `${meta.arc_id}` in the branch name for concurrent-arc-safe naming.

### Failure semantics

`on_failure` per hook decides what an op error means:

- `halt` (default) — abort the arc with the op's error.
- `warn` — log + write a `surprise` note on the arc thread, continue.
- `ignore` — log only, continue.

Hooks cannot change which next node runs — that authority stays with
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
   (race-loser cleanup — only the first signal wins).

The arc resumes, populates `ctx.last_signal` + appends to
`signal_history`, and proceeds to the gate (which can branch on
`last_signal.name`, `last_signal.payload.*`, etc.).

### Timeout

If `timeout` fires before any signal:

1. All sibling waits are removed.
2. A synthetic signal is recorded:
   `SignalRef { name: "__timeout__", payload: { expired: ["sig1", "sig2"] } }`.
3. Gate evaluates as normal (route via `Eq{field: "last_signal.name", value: "__timeout__"}`).

`timeout` accepts `30s`, `5m`, `1h`, `7d` — any decimal followed by a
unit suffix. Absent timeout = wait indefinitely (suitable for
never-firing-but-cancellable arcs).

## Subworkflows

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
on sub completion. **Missing exports are a runtime error** — if you
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

- `gate_mode: "first"` (default) — first-matching rule wins, single verdict.
- `gate_mode: "all"` — every matching rule fires, aggregate verdict is
  the lattice-highest-priority classification across all findings.
  Findings are surfaced as a `learned` note on the arc thread and
  exposed via `bro orchestrate status`.

Back-edges in the graph become natural retry loops. Each visit bumps
`visit_counts[node]`; exceeding `retry.max_generations` halts the
arc. Retried prompts get `[retry — attempt N, prior gate verdict: X]`
prepended automatically.

## Workflow-level policy packets

`policy_packet: <id>` runs at every node boundary against the
flattened ArcContext (with arc-shape additions: `step`, `just_ran`,
`next`, `completed`, `in_flight`, `visit_counts`). Classifications
are arc-level verdicts:

- `halt` — stop the arc immediately (error exit, reason posted to thread).
- `escalate` — write a `blocked` note, continue.
- `warn` — write a `surprise` note, continue.
- anything else — no-op.

This is the mechanization of the advisor loop: instead of dispatching
an LLM at every boundary to read the checkpoint and judge, compile
those rules into a packet once and let them evaluate deterministically.
Useful for runaway-visit detectors, time / step ceilings, arc-shape
invariants.

## Domain-shaped packet refs

Anywhere a workflow names a packet (`gate`, `policy_packet`,
`hook.when`, `webhook.routing_packet`), accept either:

- `packet-XXXXXXXX` — pinned exact compile.
- `domain:<name>` — resolves to the most-recently-compiled packet
  matching `<name>`.

Domain refs let workflow + webhook specs survive packet recompiles
without re-edit. Audit events name the resolved `packet-id` so you
always know which compile actually fired.

## Webhook ingress

Webhooks are operator-installed (signature scheme + Extractor +
routing packet). The pipeline:

```text
HTTP POST /webhook/<name>
  → verify_signature(secret)         [HmacSha256 (configurable header
                                       + optional `prefix=`) | None
                                       (loopback-bind only)]
  → idempotency dedup                [delivery-id ring per webhook]
  → extractor.project(payload)       [JSON → flat entity]
  → routing_packet.apply(entity)     [verdict]
  → dispatch_verdict(...)            [start_arc | signal_arc | cancel_arc | ignore | dead_letter]
```

### Extractor

Tiny AST that projects a webhook payload (plus `_headers` map for
header-driven routing — operator names the header in their extractor;
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
shapes — e.g. Forgejo's `pull_request_review` puts the comment text
at `.review.body` while `pull_request_review_comment` uses
`.comment.body`). Deliberately small — no transformations (regex,
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
   - `None` (worktree hooks fail loudly — better than silent fallback to cwd).
5. Spawns the arc in a background task. Webhook returns immediately
   with `{status: "arc_started", workflow: <id>}`.

### Default-deny

No-match on the routing packet is **dead-lettered**, not ignored.
The extracted entity is included in the response for forensics.

### Replay

POST `/webhook/<name>/replay` with the same payload runs extractor →
routing packet → verdict **without dispatching**. Use to debug routing
rules without firing arcs.

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

Every `bro orchestrate run` opens a `bbox_thread(kind=work_item)`. The
returned `arc_thread_id` is the audit handle.

| Surface                                  | What it shows                                                           |
|------------------------------------------|-------------------------------------------------------------------------|
| `bro orchestrate peek [<thread-id>]`     | Live in-flight state from the `running_arcs` registry. No id = all.    |
| `bro orchestrate status <thread-id>`     | Note trail + latest compaction anchor for one arc.                     |
| `bro orchestrate list [--limit N]`       | Recent arcs catalog with final status + anchor.                        |
| `bbox_notes(thread_id=<arc>)`            | Every structured note the engine wrote (kind: done / learned / surprise / blocked). |
| `bbox_inbox`                              | Arcs currently flagged for attention (failed, blocked, etc.).          |
| `bro_arc_status(arc_id=<id>)`            | Snapshot + pending-wait registrations for one arc.                     |
| HTTP `GET /tail` (SSE)                   | Live event stream from the daemon — every node_dispatch / hook_ok / wait_registered / gate_applied / ... |

Compaction anchors: at every node boundary the engine writes a
rolling `ANCHOR [step N, just-ran='X', next='Y']: completed=[...]
in_flight=[...] verdict=Z visits=[...]` note. Lets observers
reconstruct arc state without reading every event.

## Lifecycle

```text
spawn arc
  → seed initial_vars (schema-validated)
  → open arc thread (bbox_thread kind=work_item)
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
  extracted entity before verdict parse) — same `${X}` shape the
  workflow templater uses, applied to a different scope
- Two convergent event inlets: webhook ingress (signed inbound POST)
  and poller (scheduled HTTP fetch). Both extract → route → dispatch
  through one shared `dispatch_routed_event` so a workflow doesn't
  care how it was triggered. Pollers reuse `HttpFetchSpec` (same
  primitive the `http_json` op consumes), explode array responses
  via `iterate`, dedup by operator-named id field, and resolve
  `${env.X}` in source URL/headers per-tick (no secrets persisted
  to disk).
- Hook lifecycle: on_enter, on_exit, on_arc_exit, on_arc_cancel
- Op catalog: SetVar, IncVar, AppendVar, MergeVar, ParseJson, Shell,
  WorktreeCreate (with smart branch reuse), WorktreeRemove, SetMeta,
  HttpJson (generic HTTP; `response_kind: json|text|auto`),
  FindFirst (array search by dotted-path equality — composable
  primitive for idempotent lookups). Engine carries no platform
  knowledge; workflow author composes generic ops + `${env.X}` +
  `${vars.X}` to express any code-host integration.
- Hook gating via `when: domain:...` + `on_failure: halt|warn|ignore`
  (gate-packet errors route through `on_failure` too — no silent skips)
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
- Actor kinds: executor, ensemble, advisor, user (plus the empty
  `actor` form for hook-only nodes)
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

## Phase-next

- `bro orchestrate resume <thread-id>` — genuine re-entry at the last
  recorded step for arcs that paused or errored. Needs persistent
  full-output snapshots to survive daemon restarts.
- Disk-backed `WaitStore` (currently in-memory) so suspended arcs
  survive daemon restart.
- Disk-backed `running_arcs` registry so `peek` works after a restart.
- YAML workflow loader (JSON-only today; one-line add once `serde_yaml`
  is introduced).
- `cancel_arc` routing verdict actually wired through.
- Push-storm debouncing (real deployments want a `vars.review_in_progress`
  flag the routing packet checks). Pattern documented; not yet wired
  in the keystone example.
- Sandboxed `Shell` op gated via packet policy (today: shell runs
  with `cwd` from `meta.worktree` but no allowlist enforcement).

## Examples

- [`examples/keystone/`](examples/keystone/README.md) — end-to-end:
  Forgejo webhook → arc → implementer subworkflow → wait → reviewer
  ensemble → wait-loop until merged → cleanup hooks. Real LLM
  dispatch. Full layout + adaptation guide in the example's README.
- [`examples/workflows/`](examples/workflows/README.md) — smaller
  shape catalog: linear, gated, ensemble, fork-join, blind-convergence,
  optimistic-review, self-audit.

## Authoring loops

If you don't want to hand-write specs:

- `bro_orchestrate_author(charter, brofile, hint?)` — prose-charter
  to validated spec via an authoring LLM. Auto-retries on compile
  failure with the error appended.
- Always pair with `bro_orchestrate_run(workflow, dry_run=true)` to
  validate before dispatching.

For runtime debugging:

- `POST /webhook/<name>/replay` with the actual payload to inspect
  what the routing packet would do, no arc spawned.
- `bro_arc_status(arc_id=<id>)` for the current snapshot + pending waits.
- `bbox_notes(thread_id=<arc>)` for the audit trail.

# System Events

Date: 2026-05-13
Status: shipped

Related:
- [WORKFLOWS](../../WORKFLOWS.md) - current workflow hooks, inbound
  webhook/poller/cron inlets, Wait signals, and routing packets.
- [Atom System](atom-system.md) - reusable capability boundary; reactions may
  invoke atoms as bounded actions.
- [bro-slack next](bro-slack-next.md) - Slack egress and native agent
  projection; system events should become its daemon-side signal source.
- [Supervision](supervision.md) - mid-dispatch observations and policy-driven
  action over running work.
- [Keystone example](../../examples/keystone/README.md) - existing
  Forgejo-backed issue to PR to review loop.

## Problem

Blackbox has the inbound half of evented coordination:

- webhooks accept signed external events
- pollers synthesize events from scheduled HTTP fetches
- crons synthesize time-based events
- whiteboard phase transitions can emit direct routed signals
- workflow Wait nodes suspend until matching signals arrive

Blackbox also has local side-effect slots:

- workflow hooks run `http_json`, `mcp_call`, `shell`, variable updates, and
  related ops at node and arc lifecycle boundaries
- atoms package reusable capabilities behind typed contracts
- `/tail` streams task lifecycle events to live subscribers

Those pieces do not add up to a daemon-wide outbound event model.

The missing piece is a first-class way for Blackbox itself to say "this system
fact happened" and for operator-installed reactions to perform audited,
idempotent outbound work in response. Workflow hooks are scoped to one arc.
Inbound webhooks are scoped to external callers. Tail events are live
observation only. None of them are the right owner for cross-cutting daemon
activities such as provisioning coordination-plane identities, updating issue
status, writing audit comments, synchronizing external dashboards, or enforcing
system-wide lifecycle policy.

## Thesis

**System events are durable daemon facts; reactions are policy-gated outbound
effects over those facts.**

An event says what happened. A reaction decides what to do about it. The event
producer must not know whether Forgejo, Slack, ntfy, a workflow, or an atom is
listening.

The target pipeline:

```text
daemon activity
  -> emit SystemEvent
  -> append durable event record
  -> enqueue matching reaction attempts
  -> evaluate packet gate against event envelope
  -> execute bounded action
  -> record success / retry / dead-letter
  -> optionally emit follow-up SystemEvent
```

This is intentionally the outbound sibling of inbound inlets:

```text
inbound:  external event -> extractor -> routing packet -> dispatch verdict
outbound: system event   -> reaction  -> packet gate    -> bounded action
```

## Vocabulary

### SystemEvent

A **SystemEvent** is an append-only fact emitted by the daemon. It is not a
command and not a hook declaration.

Canonical shape:

```jsonc
{
  "id": "evt-...",
  "kind": "bro.identity.required",
  "occurred_at": "2026-05-13T12:34:56Z",
  "producer": "orchestration.dispatch",
  "project": "/home/invidious/repos/transcript-search",
  "principal": {
    "kind": "bro",
    "bro": "keystone-review",
    "provider": "claude",
    "model": "haiku-4.5",
    "effort": "medium"
  },
  "subject": {
    "kind": "bro",
    "id": "bro:keystone-review"
  },
  "correlation": {
    "task_id": "task-...",
    "arc_id": "thread-..."
  },
  "causation_id": "evt-...",
  "payload": {}
}
```

Required fields:

- `id` - durable event id, generated once.
- `kind` - stable dotted event kind.
- `occurred_at` - event time in UTC.
- `producer` - subsystem that emitted it.
- `payload` - event-specific JSON object.

Optional but common fields:

- `project` - absolute project path when scoped.
- `principal` - attribution principal that caused or owns the event.
- `subject` - entity the event is about.
- `correlation` - task, arc, thread, PR, issue, board, session, or external ids.
- `causation_id` - parent event id when this event is derived from another.

Do not call this field `actor`. In Blackbox, workflow actors are executable
node participants; a system-event principal is only attribution metadata and
must not imply a new runtime actor model.

Events are immutable. Corrections are new events. The journal record has a
single envelope version (`system-event/v1`) at the storage/serialization layer;
packet gates evaluate the event fields, not the storage wrapper.

### Reaction

A **Reaction** is an operator-installed subscription from system events to
bounded actions.

Example:

```jsonc
{
  "_contract": "reaction/v1",
  "name": "forgejo-provision-bro-user",
  "version": 1,
  "enabled": true,
  "event_kinds": ["bro.identity.required"],
  "when": "domain:system-event/forgejo-identity-required",
  "idempotency_key": "forgejo:${event.payload.instance}:${event.subject.id}",
  "action": {
    "op": "http_json",
    "args": {
      "method": "POST",
      "url": "${env.FORGEJO_BASE_URL}/api/v1/admin/users",
      "headers": {
        "Authorization": "token ${env.FORGEJO_ADMIN_TOKEN}",
        "Content-Type": "application/json"
      },
      "body": {
        "username": "${event.payload.username}",
        "full_name": "${event.payload.display_name}",
        "email": "${event.payload.email}"
      },
      "expect_status": [200, 201, 422]
    }
  },
  "retry": {
    "max_attempts": 5,
    "backoff": "exponential"
  },
  "on_failure": "dead_letter"
}
```

The reaction owns:

- event-kind subscription
- packet gate
- idempotency key
- action declaration
- retry policy
- failure policy

It does not own event production.

### Template Scope

Reaction templates reuse the workflow template resolver with a new root:
`event`. Available roots are:

- `${event.*}` - full SystemEvent envelope.
- `${env.X}` - environment variables, resolved the same way workflow
  `http_json` args resolve them.

No other implicit roots are available in v1. Unresolved template expressions
are hard errors. They do not fall through to literal text, because the
idempotency key is the correctness boundary for outbound effects.

The `idempotency_key` is rendered before an outbox item is claimed. If it
cannot be rendered, the item is marked `dead_lettered` with reason
`idempotency_template_error`; no action runs.

### Packet Entity

The reaction `when` packet evaluates against a flattened event entity. The
entity is deterministic and contains:

- `kind`, `producer`, and `project`
- `principal.*` fields when present
- `subject.*` fields when present
- `correlation.*` fields when present
- every `payload.*` field

`id`, `occurred_at`, and journal/envelope metadata are omitted by default so
packet rules stay stable and replayable. A replay/debug mode may include them
under `_meta.*`, but production gates should not depend on timestamps or event
ids unless a later use case forces that.

### Outbox

The **outbox** is the durable execution ledger for `(event, reaction)` pairs.
It is separate from the event journal because one event may drive zero, one, or
many reactions.

Outbox records carry:

- event id
- reaction ref
- idempotency key
- status: `pending`, `claimed`, `succeeded`, `retry_at`, `dead_lettered`
- attempt count
- last error
- dead-letter reason when terminal failure is policy, recursion, validation, or
  retry exhaustion
- last response summary
- created/updated timestamps

The outbox is the reliability boundary. Live broadcast is optional
observability; outbound effects must not depend on live subscribers being
present.

Side-effecting reactions must have an idempotency key. This is not only a
dedupe convenience; it is the crash-safety contract. If the daemon crashes
after executing an external side effect but before recording success, restart
recovery may re-run the action. The external operation must therefore be safe
to repeat under the same idempotency key or the outbox must refuse to run it.

Startup recovery:

- `pending` and due `retry_at` records are eligible to claim.
- stale `claimed` records from a prior daemon process are returned to
  `pending` only when an idempotency key is present.
- stale `claimed` records without an idempotency key are marked
  `dead_lettered` with reason `crash_recovery_non_idempotent`.

### Reaction Ordering

V1 runs one outbox worker and claims records in journal order, then reaction
name order for reactions produced by the same event. This gives deterministic
replay without pretending that reactions can depend on each other implicitly.

Reactions should not rely on another reaction to the same event having already
run. If one side effect must causally follow another, the first reaction should
emit a derived event and the second reaction should subscribe to that derived
event.

## Event Kinds

Initial event families should be small and boring.

### Bro Identity

```text
bro.identity.required
bro.identity.provisioned
bro.identity.provision_failed
```

Use when a bro needs an external coordination-plane principal.

Payload for `bro.identity.required`:

```jsonc
{
  "identity_scope": "forgejo",
  "instance": "local-forgejo15",
  "bro": "keystone-review",
  "provider": "claude",
  "model": "haiku-4.5",
  "effort": "medium",
  "username": "bro-keystone-review-claude-haiku45",
  "display_name": "keystone-review / claude haiku-4.5 medium",
  "email": "bro-keystone-review@blackbox.local"
}
```

### Task Lifecycle

```text
task.started
task.progress
task.completed
task.failed
task.cancelled
```

These can be emitted from the same places that currently feed `/tail`, but
the payload should be richer and durable. `/tail` can eventually become a
projection over system events for task lifecycle. V1 should persist only
milestone progress: task start, task completion/failure/cancel, explicit
`bro_report`-style progress, and workflow node boundaries. Raw provider stream
chunks and token deltas stay out of the journal.

### Workflow Lifecycle

```text
workflow.arc.started
workflow.arc.node_started
workflow.arc.node_completed
workflow.arc.wait_registered
workflow.arc.signal_received
workflow.arc.completed
workflow.arc.failed
workflow.arc.cancelled
```

These events should mirror the important `WorkflowRunner::log_event` facts but
be promoted to a typed daemon envelope where reactions can consume them.

### Coordination Plane

```text
coordination.issue.linked
coordination.pr.opened
coordination.review.posted
coordination.status.changed
coordination.audit_comment.requested
```

These are optional higher-level events emitted by Forgejo/Slack/whiteboard
adapters or workflows when they want a generic coordination-plane reaction.
Keep them generic enough that Forgejo is one adapter, not the daemon model.

### Whiteboard And Council

```text
whiteboard.phase_changed
whiteboard.vote_recorded
council.posted
council.mention
```

Whiteboard phase changes already dispatch routed signals. System events should
not replace that. They provide audit and egress hooks around the same facts.

## Forgejo Per-Bro Identity

The immediate use case is a Forgejo instance acting as a coordination plane.
Each bro should be able to appear as a distinct Forgejo user so that:

- Forgejo review self-approval rules are no longer blocked by a single shared
  user.
- PR comments, reviews, labels, and merges have a useful audit trail.
- The identity can encode provider/model details, while effort stays on
  per-dispatch events and audit comments.
- Coordination-plane activity can be traced back to Blackbox task/session/arc
  ids.

The proposed flow:

```text
dispatch wants Forgejo principal for bro
  -> identity registry lookup
  -> missing mapping emits bro.identity.required
  -> reaction provisions or finds Forgejo user/token
  -> reaction writes identity mapping
  -> emits bro.identity.provisioned
  -> dispatch/workflow obtains principal for Forgejo API calls
```

Identity mapping key:

```text
(identity_scope, instance, bro_id, provider, model)
```

Effort is not part of the durable identity key in v1. It changes too often and
would multiply external users without improving attribution enough to justify
the operational cost. Store effort on system events, action metadata, and audit
comments instead.

Principal record:

```jsonc
{
  "scope": "forgejo",
  "instance": "local-forgejo15",
  "subject": "bro:keystone-review",
  "provider": "claude",
  "model": "haiku-4.5",
  "external_user_id": 123,
  "username": "bro-keystone-review-claude-haiku45",
  "token_ref": "secret:forgejo-bro-keystone-review-claude-haiku45",
  "created_at": "...",
  "last_verified_at": "..."
}
```

Token material belongs in the existing secrets mechanism or environment-backed
secret refs, not in the identity mapping JSON. V1 `secret:` refs use existing
secret names, which allow `[A-Za-z0-9_.-]+` and reject path separators; scoped
secret namespaces can be added later behind a new resolver instead of smuggling
slashes into the current name format.

## Relationship To Existing Primitives

### Workflow Hooks

Hooks remain the right primitive for side effects local to an arc:

- create/remove worktrees
- fetch issue data
- push branch
- open or update the PR for this arc
- post review result computed inside this arc

System reactions are the right primitive for cross-cutting daemon behavior:

- ensure this bro has a Forgejo identity
- mirror task status to a coordination plane
- record an audit comment for every merge by a bro
- update a dashboard when any workflow blocks
- emit Slack/ntfy notification when any arc dead-letters

The boundary: if the workflow author must name the effect for the workflow to
make sense, it is probably a hook. If every workflow would want the same effect
because the daemon activity happened, it is probably a reaction.

### Inbound Webhooks

Inbound webhooks convert external facts into Blackbox routing verdicts.
System events convert Blackbox facts into external effects.

They should share concepts where useful:

- extractor/template language
- packet gates
- delivery logs
- replay tools
- dead-letter inspection

They should not share names that confuse direction. Use `reaction`, not
`outbound_webhook`, as the primary artifact kind.

### Atoms

Reactions may invoke atoms when the outbound effect is a reusable capability
with a contract. Examples:

- `atom:forgejo-ensure-user@v1`
- `atom:forgejo-post-status@v1`
- `atom:slack-stream-task-update@v1`

Small deterministic HTTP effects can be direct reaction actions. Complex
effects with validation, retries, or multi-step behavior should become atoms.

### Tail

The current tail path is an in-process `tokio::broadcast` stream of
`TailEvent`. That is appropriate for live observation and SSE.

System events should use the same Rust idiom for live fanout:

```rust
tokio::sync::broadcast::Sender<SystemEvent>
```

but durable reactions must read the journal/outbox, not the broadcast channel.
Slow subscribers may lag; outbound effects must not be dropped because a
subscriber lagged.

### Notes And Inbox

Notes are human/operator side-channel observations. System events are machine
facts. Do not replace one with the other.

Dead-lettered reactions surface into the inbox by emitting a structured
`bbox_note(kind="blocked")` scoped to the event project when available. The
note body carries reaction name, event id, event kind, outbox id, dead-letter
reason, and redacted error summary. The source of truth remains the outbox
record; the note is the attention-layer projection.

## Storage

Use the existing Blackbox state-dir pattern:

```text
${BRO_HOME}/events/journal/*.jsonl
${BRO_HOME}/events/outbox/*.jsonl
${BRO_HOME}/reactions/<name>.json
${BRO_HOME}/identities/<scope>/<instance>.json
```

The first implementation uses append-only JSONL plus copy-forward compaction,
matching the rest of the daemon's file-backed bias. Startup runs the same
compaction once, `system_event_compact` exposes the manual ops surface, and the
installable `daily-compaction` system-default cron runs system-event compaction
as part of the cross-store maintenance flow. If event volume becomes large,
move the journal/outbox to SQLite or another embedded store behind the same
trait.

Retention defaults:

- event journal: keep 10,000 events or 7 days, whichever limit is hit first.
- outbox: keep all non-success records; compact successful records older than
  7 days to `{event_id, reaction, idempotency_key, succeeded_at, response_hash}`.
- identity registry: durable until explicitly retired.

JSONL compaction is copy-forward: write a compacted temp file containing the
retained records/summaries, fsync, then rename over the old segment. Do not
rewrite in place.

The bundled daily maintenance flow (`system-defaults/maintenance`) starts
`daily-compaction-arc`, which runs system-event compaction, edge storage GC, and
vector partition compaction through the existing embed compaction policy.

## Reaction Action Catalog

Start with a deliberately small action catalog:

| Action | Purpose |
|---|---|
| `http_json` | Generic HTTP JSON/text request, same semantics as workflow hook op where possible. |
| `mcp_call` | Call an installed MCP tool through the registry. |
| `atom_invoke` | Invoke an installed atom with event-derived args. |
| `start_workflow` | Start a workflow from a system event, equivalent to inbound `start_arc`. |
| `emit_event` | Emit a derived system event after a reaction step. |

Do not include arbitrary shell in v1 reactions. Shell belongs in workflow hooks
where cwd/worktree/policy context is explicit.

## Policy And Safety

Every reaction is default-deny:

- disabled unless installed by the `ops` surface or an equivalent admin-only
  HTTP endpoint
- must name event kinds explicitly
- packet gate is optional syntactically but strongly recommended; admin tools
  should warn when absent
- action catalog is closed
- idempotency key is required for all side-effecting actions and all actions
  that can cross a process boundary
- retries are bounded
- dead letters are inspectable and replayable

Secret handling:

- reaction specs may reference `${env.X}` or `secret:` refs; v1 `secret:` refs
  use existing flat secret names and therefore cannot contain `/` or `\`
- resolved secret values are never written to event journal or outbox response
  summaries
- response redaction should run before storing error bodies

Recursion guard:

- reactions that start workflows, invoke atoms, or emit derived events must
  carry a recursion budget
- derived events carry `causation_id`
- event emission walks the `causation_id` ancestry and rejects any derived
  event whose depth would exceed 4
- the outbox also rejects a repeated `(event.kind, reaction.name)` pair in the
  same causation ancestry
- rejected chains are marked `dead_lettered` with reason
  `recursion_budget_exceeded`; no action runs

MCP surface policy:

| Tool | Minimum surface |
|---|---|
| `reaction_install` | `ops` |
| `reaction_retry` | `ops` |
| `reaction_execute` | `ops` |
| `system_event_emit` | `ops` |
| `reaction_replay dry_run` | `default` |
| `reaction_list` / `reaction_deliveries` | `default` |
| `system_event_list` / `system_event_open` | `readonly` |
| `identity_list` / `identity_get` | `readonly` |

## Replay And Observability

Add operator tools analogous to webhook replay/deliveries:

```text
system_event_emit        # admin/dev: inject synthetic event
system_event_list        # recent event journal
system_event_open        # one event + derived/caused records
reaction_install         # install reaction spec
reaction_list            # installed reactions
reaction_replay          # evaluate one event against one reaction, dry-run by default
reaction_execute         # execute one event/reaction through an audited outbox row
reaction_deliveries      # recent outbox attempts/results
reaction_retry           # retry a dead-lettered delivery
identity_list            # list known external principals
identity_get             # inspect one mapping
```

HTTP equivalents can exist under `/admin/reaction/*`, but MCP tools should be
the primary operator surface.

Replay and execute modes:

- `reaction_replay`: render templates, evaluate gate, do not execute action.
- `reaction_execute(force=false)`: execute action with the rendered
  idempotency key and normal succeeded-idempotency suppression.
- `reaction_execute(force=true)`: bypass succeeded-idempotency suppression,
  admin-only. Gates, causation guards, redaction, and outbox audit still apply.

The side-effecting operation is a separate ops-only tool because MCP surfaces
are tool-level, not parameter-level; exposing `execute` as a mode on the
default-visible dry-run tool would leak writes to read-only callers.

## Implementation Sketch

### 1. Event Types

Add a `system_events` module with:

```rust
pub struct SystemEvent { ... }
pub enum SystemEventKind { ... }
pub struct EventPrincipal { ... }
pub struct EventSubject { ... }
```

Keep the wire kind dotted and stable. The enum can map to/from strings.

### 2. Event Bus

Add to shared state:

```rust
pub(crate) system_events: system_events::SharedEventHub,
```

The hub owns:

- `broadcast::Sender<SystemEvent>` for live subscribers
- durable journal writer
- reaction registry reference or callback
- outbox enqueue path

The producer API should be small:

```rust
state.system_events.emit(SystemEventDraft { ... }).await?;
```

The draft gets id/timestamp filled centrally and is written through the
`system-event/v1` journal envelope.

### 3. Reaction Registry

Persist reaction specs under `${BRO_HOME}/reactions`. Restore on daemon
startup the same way webhooks, pollers, crons, and workflows restore.

Compile-time validation:

- schema contract
- known action op
- event kinds non-empty
- idempotency key present for `http_json`, `mcp_call`, `atom_invoke`, and
  `start_workflow`
- referenced packet exists when using a concrete packet id; domain refs resolve
  at runtime like workflows

### 4. Outbox Runner

Run a background worker:

```text
loop:
  claim next due outbox item
  load event + reaction
  render idempotency key
  evaluate gate
  execute action
  store result
  emit derived event if configured
```

Use per-item locking or atomic file rename to avoid double claims if multiple
workers are introduced later. V1 can run a single daemon-local worker.

On daemon startup, the runner performs recovery before claiming new work:

1. Requeue stale `claimed` items with idempotency keys.
2. Dead-letter stale `claimed` items without idempotency keys.
3. Leave `succeeded` records untouched.
4. Leave `dead_lettered` records untouched until an operator calls
   `reaction_retry`.

Build this runner against synthetic events before wiring real emit sites; it is
the reliability core of the design.

### 5. Emit Sites

Initial emit sites:

- bro dispatch start/completion/failure/cancel
- workflow arc start/completion/failure/cancel
- workflow Wait registration/resolution
- whiteboard phase transition
- identity registry miss

Avoid emitting every low-level provider stream chunk in v1. Keep volume low.

### 6. Forgejo Identity Adapter

Add an identity registry plus one or more reaction/atom examples:

- `reaction:forgejo-ensure-bro-user@v1`
- `atom:forgejo-ensure-user@v1` if the behavior is multi-step
- `reaction:forgejo-task-status@v1` for task/arc progress projection

The identity lookup path should be usable by Keystone-style workflows without
embedding user provisioning logic in each workflow JSON.

## Resolved V1 Decisions

1. Reaction specs start as a standalone registry like webhooks/crons. Long
   term, they can become an artifact-catalog kind.

2. Do not index the event journal into the agentic corpus in v1. Revisit after
   retention and redaction have production mileage.

3. Only milestone `task.progress` is durable. Raw token or provider stream
   events are too noisy.

4. Identity username templates are configured by identity scope/instance; do
   not hardcode Forgejo-specific naming into the daemon core.

5. Reaction actions share lower-level executors (`http_json`, `mcp_call`)
   where useful, but keep reaction schema separate from workflow `HookOp`.

## Acceptance Criteria

The first useful slice is complete when:

- the daemon can emit and persist `bro.identity.required`
- an installed reaction can provision or verify a Forgejo user
- the resulting identity mapping is durable and inspectable
- repeated dispatches for the same bro do not create duplicate users
- failures land in a visible dead-letter/outbox surface
- Forgejo outage causes bounded retries, then a dead-letter with a redacted
  error summary and a blocked inbox note
- retrying the dead-letter succeeds after Forgejo recovers without creating a
  duplicate user
- a Keystone-like workflow can use distinct Forgejo principals without
  embedding identity provisioning hooks in every arc

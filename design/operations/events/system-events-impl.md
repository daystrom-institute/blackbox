---
title: "System Events \u2014 Implementation Plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - operations
  - events
---

# System Events — Implementation Plan

Date: 2026-05-13
Status: shipped
Companion to: [System Events](system-events.md)

Related:
- [WORKFLOWS](../../../WORKFLOWS.md) - workflow hooks, inlets, Wait signals,
  routing verdicts, and operational surfaces.
- [Atom System Impl](../../orchestration/atoms/atom-system-impl.md) - artifact/ref/runtime patterns for
  reusable invocation surfaces.
- [Supervision Impl](../../orchestration/supervision/supervision-impl.md) - per-event daemon observations,
  policy evaluation, and action routing.
- [Keystone](../../../examples/keystone/keystone-example.md) - Forgejo-backed coordination
  example this work extends.

## Implementation Thesis

Build the reliability substrate before wiring real producers.

The tempting path is to start at the Forgejo identity use case. That would hide
the hard part behind a demo. The load-bearing work is the event journal,
reaction registry, idempotency rendering, crash recovery, retry, dead-letter
visibility, and replay. Forgejo identities should be the first end-to-end
consumer only after synthetic events can prove the outbox behaves correctly
through failures.

Core build order:

```text
types + stores
  -> event hub + synthetic emit
  -> reaction registry + dry-run replay
  -> outbox runner + crash recovery
  -> action executors
  -> real emit sites
  -> Forgejo identity registry/reaction
  -> docs/examples/surface hardening
```

## Phase DAG

```text
Phase 0 ─▶ Phase 1 ─▶ Phase 2 ─▶ Phase 3 ─▶ Phase 4 ─▶ Phase 5 ─▶ Phase 6
             │                                           │
             └────────────── secret_write ───────────────┘
                                                         ▼
                                                      Phase 7
                                                         ▼
                                                      Phase 8
```

Phase 0 is grounding only. Phase 1 creates types and durable stores. Phase 2
adds event emission and synthetic event tools. Phase 3 adds reaction registry
and dry-run replay. Phase 4 builds the outbox runner and crash recovery. Phase
5 adds action executors. Phase 6 wires real emit sites. Phase 7 implements the
Forgejo identity path and depends on Phases 1-5, including the Phase 1
secret-write primitive. Phase 8 finishes docs, examples, and operational
hardening.

Every mutating Rust phase should end with:

```text
rtk cargo fmt
rtk cargo test --bin blackboxd
```

Use narrower tests while developing, but do not claim a phase complete without
the binary test suite.

---

## Phase 0: Baseline And Inventory

**Prerequisites:** none.

**What gets built:** no product behavior. This phase pins the existing seams
before new code is introduced.

0.1 **Inventory current event surfaces.**

Read and record the code anchors:

- `src/orchestration/tail.rs` - current live task event enum.
- `src/server/state.rs` - shared `tail_tx`, registries, and logs.
- `src/main.rs` - daemon startup, registry restore, routes, and
  `dispatch_routed_event`.
- `src/webhooks.rs`, `src/pollers.rs`, `src/crons.rs` - inbound registry
  patterns.
- `src/workflow/engine.rs` - `log_event`, Wait registration/resolution, arc
  lifecycle.
- `src/tools/orchestrate.rs` - existing admin install/list/replay tool style
  for webhooks, pollers, crons, and workflows.
- `src/json_store.rs` - locked atomic JSON write helper.
- `src/secrets.rs` - flat secret-name validation.
- `src/server/surface.rs`, `src/tools/mcp_surface.rs`,
  `system-defaults/mcp-surfaces/routing.json` - MCP surface model.
- `AGENTS.md` / rendered project docs - check whether any build/run
  instructions are stale before relying on them. At the time this plan was
  written, the local crate already had a `[lib]` target even though older
  project instructions still described it as bin-only.

0.2 **Baseline tests.**

Run:

```text
rtk cargo test --bin blackboxd
```

Record any pre-existing failures before editing. New failures after Phase 1 are
owned by this work.

0.3 **Choose storage shape for v1.**

V1 uses append-friendly JSON files plus locked compaction:

```text
${BRO_HOME}/events/journal/current.jsonl
${BRO_HOME}/events/outbox/current.jsonl
${BRO_HOME}/events/outbox/dead-letter.jsonl
${BRO_HOME}/reactions/<name>.json
${BRO_HOME}/identities/<scope>/<instance>.json
```

Do not introduce SQLite in v1. The file-backed implementation keeps this
consistent with existing registry and state-dir patterns.

**Deliverable:** no committed code required, but the implementer has concrete
file anchors and a clean or documented test baseline.

---

## Phase 1: Types, Stores, And Validation

**Prerequisites:** Phase 0.

**What gets built:** `system_events` module with typed envelopes, reaction
specs, identity records, durable store APIs, and validation.

1.1 **Add module skeleton.**

New files:

- `src/system_events/mod.rs`
- `src/system_events/types.rs`
- `src/system_events/store.rs`
- `src/system_events/hub.rs`
- `src/system_events/outbox.rs`
- `src/system_events/reactions.rs`
- `src/system_events/identity.rs`
- `src/system_events/template.rs`

Follow the existing feature-module pattern: declare `mod system_events` in
`src/main.rs`. Do not add it to `src/lib.rs` unless a separate library consumer
needs these types. The current library target exposes utility modules; the
daemon feature module belongs with webhooks, pollers, crons, and workflow
runtime code.

1.2 **SystemEvent types.**

Add:

```rust
pub struct SystemEvent {
    pub id: String,
    pub kind: SystemEventKind,
    pub occurred_at: String,
    pub producer: String,
    pub project: Option<String>,
    pub principal: Option<EventPrincipal>,
    pub subject: Option<EventSubject>,
    pub correlation: serde_json::Map<String, serde_json::Value>,
    pub causation_id: Option<String>,
    pub payload: serde_json::Value,
}
```

Use dotted wire strings for `SystemEventKind`. The enum can be internally
typed, but serde must round-trip unknown future kinds as strings or a
non-exhaustive variant. Reactions should not break when a newer daemon writes a
kind the current binary does not consume.

Do not introduce `EventActor` or an `actor` field. The repo already uses
workflow actors for executable workflow participants. System events only need
an optional `EventPrincipal` attribution field; it is metadata, not a runtime
actor.

1.3 **Event ids and time.**

Use existing dependencies:

- `uuid::Uuid::new_v4()` for `evt-<uuid>` and `outbox-<uuid>`.
- `crate::util::now_iso()` for UTC timestamps.

Do not add a new id/time dependency.

1.4 **Journal envelope.**

Store each line as:

```jsonc
{
  "schema": "system-event/v1",
  "event": { ... SystemEvent ... }
}
```

The event itself does not carry a `schema` field. Packet evaluation never sees
the storage wrapper.

1.5 **ReactionSpec types.**

Add:

```rust
pub struct ReactionSpec {
    pub contract: String,
    pub name: String,
    pub version: u32,
    pub enabled: bool,
    pub event_kinds: Vec<String>,
    pub when: Option<String>,
    pub idempotency_key: Option<String>,
    pub action: ReactionAction,
    pub retry: RetryPolicy,
    pub on_failure: FailurePolicy,
}
```

Closed `ReactionAction` variants for v1:

- `HttpJson`
- `McpCall`
- `AtomInvoke`
- `StartWorkflow`
- `EmitEvent`

Do not include shell.

1.6 **OutboxRecord types.**

Add:

```rust
pub enum OutboxStatus {
    Pending,
    Claimed,
    Succeeded,
    RetryAt,
    DeadLettered,
}
```

Record fields:

- `id`
- `event_id`
- `reaction_name`
- `idempotency_key`
- `status`
- `attempt_count`
- `next_attempt_at`
- `claimed_at`
- `claimed_by`
- `last_error`
- `dead_letter_reason`
- `response_summary`
- `created_at`
- `updated_at`

`claimed_by` can be a process id plus daemon start timestamp. It only needs to
distinguish current-process claims from stale prior-process claims.

1.7 **Identity records.**

Add identity structs in `identity.rs`:

```rust
pub struct ExternalIdentity {
    pub scope: String,
    pub instance: String,
    pub subject: String,
    pub provider: String,
    pub model: String,
    pub external_user_id: String,
    pub username: String,
    pub token_ref: String,
    pub created_at: String,
    pub last_verified_at: Option<String>,
}
```

Do not store `effort` in the durable identity key/record for v1. Effort is
per-dispatch metadata and changes too often; keep it on system events and audit
comments. The same bro/provider/model identity is reused across effort levels.

Use flat `secret:` refs only. Validate that `token_ref` after `secret:` matches
the existing secret-name grammar (`[A-Za-z0-9_.-]+`, no path separators).

1.8 **Secret write primitive.**

Forgejo identity provisioning needs to store newly created external API tokens.
The current secrets module resolves secrets but does not write them. Add a
small deliberate write API to `src/secrets.rs`:

```rust
pub fn write_file_secret(name: &str, value: &str, sources: &SecretSources) -> Result<()>
```

Semantics:

- validate `name` with the existing secret-name grammar
- create the secrets directory with mode `0700` on Unix
- write to a unique temp file with mode `0600`
- fsync the temp file
- rename over the target
- do not log the secret value

This writes only to the file-secret backend
`$XDG_DATA_HOME/blackbox/secrets/<name>`. It does not write environment
variables or systemd credentials. Operator-managed tokens can still be used,
but the Forgejo provisioning path has a concrete safe default.

1.9 **Store APIs.**

Implement minimal store traits/structs:

```rust
pub struct EventStore { root: PathBuf }
pub struct OutboxStore { root: PathBuf }
pub struct ReactionRegistry { by_name: RwLock<HashMap<String, ReactionSpec>> }
pub struct IdentityRegistry { root: PathBuf, ... }
```

The first pass can use coarse locks:

- event journal append lock
- outbox append/update lock
- reaction registry RwLock
- identity registry RwLock

Reusing `json_store::with_store_lock` is acceptable for whole-file JSON stores.
For JSONL append/update, add a small sibling helper inside `system_events`
instead of overloading `json_store.rs`.

Lock ordering for event/outbox writes:

1. Acquire the event-store lock and append the journal record.
2. Release the event-store lock.
3. Acquire the outbox-store lock and append reaction records.
4. Release the outbox-store lock.

Do not hold both file locks at once. This permits an event to be durable before
its outbox fanout is complete, so Phase 2 must return an `EmitOutcome` that
makes partial success explicit. A later repair tool can scan journal records
without outbox fanout and enqueue missing records.

1.10 **Validation.**

Validate reaction specs at install/load:

- `_contract == "reaction/v1"`
- `name` non-empty and file-name safe
- `event_kinds` non-empty
- action op is known
- side-effecting/cross-process actions require `idempotency_key`
- `retry.max_attempts` is bounded; default 5, max 20
- `when` packet ref is syntactically valid; concrete packet existence can be
  checked at install, domain refs resolve at runtime
- `secret:` refs in action args and identity records are flat names

**Deliverable:** unit-tested types and stores. No daemon emits events yet.

**Tests:**

- serde round-trip for `SystemEvent` and `ReactionSpec`
- unknown event kind round-trips or degrades without panic
- reaction validation rejects missing idempotency keys for `http_json`
- identity validation rejects `secret:forgejo/foo`
- JSONL append and reload preserve order
- `write_file_secret` writes 0600 file secrets and rejects path separators
- 100 concurrent event-store appends preserve all events and produce unique ids

**Estimated size:** 700-1,000 lines Rust including tests.

---

## Phase 2: Event Hub And Synthetic Emit

**Prerequisites:** Phase 1.

**What gets built:** central event emitter, live broadcast, journal append,
outbox enqueue, and admin/dev tools for synthetic events.

2.1 **Shared event hub.**

Add to `SharedState`:

```rust
pub(crate) system_events: system_events::SharedEventHub,
```

`SharedEventHub` owns:

- `tokio::sync::broadcast::Sender<SystemEvent>` for live subscribers, capacity
  256. This stream is lossy by design; durable consumers use journal/outbox.
- `EventStore`
- `OutboxStore`
- `ReactionRegistry`
- process instance id

2.2 **Emit API.**

Add:

```rust
pub struct SystemEventDraft { ... }

impl EventHub {
    pub async fn emit(&self, draft: SystemEventDraft) -> Result<EmitOutcome>;
}
```

The hub fills:

- `evt-<uuid>`
- `occurred_at`
- default empty `correlation`
- journal envelope

`EmitOutcome` is explicit about partial success:

```rust
pub struct EmitOutcome {
    pub event: SystemEvent,
    pub journal_appended: bool,
    pub outbox_enqueued: bool,
    pub outbox_error: Option<String>,
    pub matched_reactions: usize,
}
```

Then `emit`:

1. append event to journal
2. match enabled reactions by event kind
3. create outbox records in deterministic order by reaction name
4. broadcast event to live subscribers

If outbox enqueue fails after journal append, return `Ok(EmitOutcome {
journal_appended: true, outbox_enqueued: false, outbox_error: Some(...) })`.
Do not return `Err` for this partial-success case; callers that retry a generic
error would create duplicate event ids. Reserve `Err` for failures before the
event is durably appended.

2.3 **Causation depth helper.**

Add `EventStore::causation_chain(causation_id) -> Result<Vec<SystemEvent>>`.
This follows `causation_id` pointers through an in-memory id index populated
from the current journal segment. It never scans the whole journal per emit.
Because max depth is 4, the lookup is bounded to at most 4 event-id lookups.
Reject derived events whose depth would exceed 4.

2.4 **Repeated pair helper.**

Add an `OutboxStore` helper to detect repeated `(event.kind, reaction.name)` in
the full causation ancestry. It walks the bounded causation chain (max depth 4)
and checks each ancestor's outbox records for the same pair. This is used by
the outbox runner before executing a reaction, not by generic event emission.

2.5 **MCP tools: synthetic event read/emit.**

New tool module: `src/tools/system_events.rs`, registered in `src/tools/mod.rs`
and tool docs.

Tools:

- `system_event_emit` - ops-only; creates a synthetic event draft.
- `system_event_list` - readonly; recent events with filters.
- `system_event_open` - readonly; one event plus causation/derived links.

`system_event_emit` must enforce surface policy. Do not rely only on docs.

2.6 **HTTP admin endpoint.**

Optional in this phase:

- `POST /admin/system-event/emit`

If added, keep it ops/admin only in deployment docs. MCP is the primary
operator surface.

**Deliverable:** operator can emit a synthetic event and see it in the journal.
Matching reactions create pending outbox records once Phase 3 installs
reactions; before Phase 3, emit/list/open still work.

**Tests:**

- emit fills id/time and appends journal envelope
- live subscriber receives event
- causation depth rejects depth 5
- 100 concurrent `emit` calls preserve all events, unique ids, and non-overlap
  outbox records
- `system_event_list` contains no resolved secret values and respects filters

**Estimated size:** 450-700 lines Rust.

---

## Phase 3: Reaction Registry And Dry-Run Replay

**Prerequisites:** Phase 1. Phase 2 preferred for end-to-end testing.

**What gets built:** install/list/load reaction specs and replay a reaction
against an event without executing side effects.

3.1 **Registry restore.**

On daemon startup, load `${BRO_HOME}/reactions/*.json` similarly to webhooks,
pollers, crons, and workflows. Bad specs are logged and skipped. A bad reaction
must not prevent daemon startup.

3.2 **Install tool.**

Add:

- `reaction_install` - ops-only; validates and writes a pretty JSON spec.
- `reaction_list` - default surface; lists installed reactions and validation
  warnings.

Install is list-before-create friendly: if a reaction with the same name
exists, return the prior version/source summary and require explicit
`replace=true` or equivalent before overwrite. If this is implemented through
an existing artifact install path later, keep the same behavior.

3.3 **Template renderer.**

Extract the reusable pieces of workflow templating into a generic renderer
instead of calling `ArcContext::render_template` directly. The existing
workflow method has hardcoded roots and leaves unresolved references verbatim;
reaction templates need different roots and hard-error semantics.

Add a helper with this shape:

```rust
pub enum UnresolvedPolicy {
    LeaveVerbatim,
    HardError,
}

pub fn render_template_with_roots(
    template: &str,
    roots: &serde_json::Map<String, serde_json::Value>,
    unresolved: UnresolvedPolicy,
) -> Result<String>
```

Move or share the parser/dot-walk/stringification pieces currently used by
`ArcContext::render_template`. Workflow rendering keeps `LeaveVerbatim`.
Reaction rendering uses `HardError`.

Reaction roots:

- `event`
- `env`

Unresolved expressions are errors. Add tests for the idempotency key and action
body rendering.

3.4 **Packet entity projection.**

Implement:

```rust
fn event_packet_entity(event: &SystemEvent) -> serde_json::Value
```

Projection:

- `kind`
- `producer`
- `project`
- `principal.*`
- `subject.*`
- `correlation.*`
- `payload.*`

Exclude event id and timestamps from production gates.

3.5 **Gate evaluator.**

Implement:

```rust
fn reaction_gate_allows(state: &SharedState, reaction: &ReactionSpec, event: &SystemEvent)
    -> Result<GateDecision>
```

If `when` is absent, return allow with warning metadata. If present, resolve
domain refs at runtime and apply packet. Use the same allow-verdict set as
workflow hooks unless the design later narrows it.

3.6 **Dry-run replay.**

Add:

- `reaction_replay(mode="dry_run", event_id, reaction)`

Dry-run returns:

- rendered idempotency key
- packet entity
- gate decision
- rendered action args with secrets redacted
- whether a matching succeeded outbox record would suppress execution

Dry-run must not write outbox records and must not execute actions.

**Deliverable:** installed reaction specs can be dry-run against synthetic
events with deterministic rendered outputs.

**Tests:**

- reaction install rejects invalid schema/action/idempotency
- dry-run renders `${event.payload.instance}`
- unresolved template hard-errors
- workflow templates still leave unresolved references verbatim after the
  shared renderer extraction
- packet entity contains payload and correlation fields
- dry-run redacts `Authorization` and `secret:` values

3.7 **Operator execute/force replay.**

Add:

- `reaction_execute(event_id, reaction, force=false)`

Execution creates an outbox record with the same rendered idempotency key,
claims that exact record, runs the normal worker prechecks/action path, and
persists the resulting delivery status. `force=false` preserves normal
succeeded-idempotency suppression. `force=true` bypasses only that suppression;
gates, recursion/causation guards, redaction, retries, and dead-letter handling
still apply.

This is a separate ops-only tool because MCP surfaces are tool-level, not
parameter-level. Do not expose side-effecting replay as a mode on the
default-visible dry-run tool.

**Estimated size:** 500-800 lines Rust.

---

## Phase 4: Outbox Runner, Retry, And Dead Letter

**Prerequisites:** Phases 1-3.

**What gets built:** reliable outbox execution lifecycle with synthetic/no-op
actions first.

4.1 **Claim protocol.**

Add `OutboxStore::claim_next(now, process_id) -> Option<OutboxRecord>`.

Eligible records:

- `pending`
- `retry_at <= now`

Claim transition:

```text
pending/retry_at -> claimed {
  claimed_at: now,
  claimed_by: process_id,
  attempt_count += 1
}
```

V1 can rewrite the current JSONL segment under a store lock. The critical
property is atomic state transition, not write amplification.

4.2 **Startup recovery.**

Before starting the worker loop:

- requeue stale `claimed` records with idempotency keys
- dead-letter stale `claimed` records without idempotency keys with reason
  `crash_recovery_non_idempotent`
- leave `succeeded` and `dead_lettered` untouched

For `crash_recovery_non_idempotent`, preserve a redacted copy of the rendered
action args, `claimed_at`, `claimed_by`, and attempt count in `last_error` or a
structured `dead_letter_context` field. The operator needs enough information
to manually reconcile whether the external side effect may have completed
before the crash.

4.3 **Worker loop.**

Add a background task in `main.rs` after registry restore:

```text
loop:
  claim due outbox record
  if none: sleep short interval
  load event + reaction
  render idempotency key
  check idempotency suppression
  evaluate gate
  check recursion
  execute action
  mark succeeded/retry/dead_lettered
```

Initial action support can be `emit_event` and a deterministic no-op action for
tests. Do not implement Forgejo HTTP in this phase.

4.4 **Retry policy.**

Retry defaults:

- `max_attempts: 5`
- exponential backoff with jitter
- first retry after 5s
- cap at 10m

Permanent validation errors do not retry. Network/HTTP failures retry if the
action executor marks them retryable.

4.5 **Dead-letter projection.**

On terminal dead-letter, write:

- outbox status `dead_lettered`
- reason
- redacted error summary
- `bbox_note(kind="blocked")` scoped to event project when present

Avoid recursive note reactions in v1 by not emitting system events for this
internal blocked-note projection until recursion policy has real production
coverage.

4.6 **Delivery tools.**

Add:

- `reaction_deliveries` - default surface; list outbox records.
- `reaction_retry` - ops-only; move a dead-lettered record to pending.

`reaction_retry` should require an explicit outbox id and should not batch by
query in v1.

4.7 **Crash recovery tests.**

Unit-test the store transition without killing the process:

1. write a `claimed` record with idempotency key and stale `claimed_at`
2. run recovery
3. assert it becomes pending
4. write a `claimed` record without idempotency key
5. run recovery
6. assert it becomes dead-lettered

**Deliverable:** synthetic events + installed no-op/emit-event reactions move
through pending, claimed, succeeded, retry, and dead-letter states. Recovery is
unit-tested.

**Tests:**

- claim is single-winner under lock
- retry delay schedules `retry_at`
- max attempts dead-letters
- recursion depth dead-letters
- repeated `(event.kind, reaction.name)` ancestry dead-letters
- dead-letter writes blocked note with redacted details
- crash-recovered non-idempotent claims include redacted action context for
  manual reconciliation

**Estimated size:** 800-1,200 lines Rust.

---

## Phase 5: Action Executors

**Prerequisites:** Phase 4.

**What gets built:** production action executors for the v1 action catalog.

5.1 **Shared action result shape.**

Define:

```rust
pub struct ActionOutcome {
    pub status: ActionStatus,
    pub response_summary: serde_json::Value,
    pub retryable: bool,
    pub emitted_event: Option<SystemEventDraft>,
}
```

`ActionStatus` should distinguish success, permanent failure, retryable
failure, and gate-skip. Gate-skip marks the outbox item succeeded with a
`skipped_by_gate` response summary, not dead-lettered.

5.2 **http_json executor.**

Reuse lower-level HTTP request code from workflow `http_json` if it can be
split cleanly. Do not expose workflow `HookOp` directly to reactions.

Requirements:

- render action args before execution
- support JSON/text/auto response kind if already available in workflow helper
- respect `expect_status`
- redact request/response headers in stored summaries
- classify 429/5xx/network errors as retryable
- classify 4xx except configured success statuses as permanent by default

5.3 **mcp_call executor.**

Reuse the workflow `mcp_call` implementation if it can be factored into a
lower-level function. Requirements:

- resolve server from existing MCP registry
- timeout default 300s, configurable but capped
- tool errors are permanent unless future metadata says retryable
- result summary truncates to an operational cap

5.4 **atom_invoke executor.**

Call existing atom invocation path. Requirements:

- pass rendered args
- enforce recursion budget
- store invocation id in response summary
- do not block indefinitely on long-running atoms; either record handle and
  succeed, or define a wait mode with timeout

V1 should prefer handle-returning success over blocking.

5.5 **start_workflow executor.**

Call the same internal path used by inbound `start_arc` verdicts. Requirements:

- project_dir resolution from event project or explicit action arg
- initial vars rendered from event
- return arc/task id summary
- enforce recursion budget

5.6 **emit_event executor.**

Emit a derived `SystemEventDraft` with `causation_id` set to the triggering
event id. Requirements:

- depth check before append
- repeated-pair guard in the current reaction context
- derived payload rendered from event/action args

**Deliverable:** all v1 action kinds execute under outbox control with redacted
summaries and retry classification.

**Tests:**

- `http_json` success against local test server
- `http_json` 500 schedules retry
- `http_json` 400 dead-letters or permanent-fails according to policy
- `emit_event` creates causation chain
- `start_workflow` dry-run or tiny workflow smoke
- `atom_invoke` returns invocation handle

**Estimated size:** 700-1,100 lines Rust.

---

## Phase 6: Real Emit Sites

**Prerequisites:** Phases 2-5.

**What gets built:** daemon subsystems emit real system events.

6.1 **Task lifecycle.**

Emit:

- `task.started`
- `task.completed`
- `task.failed`
- `task.cancelled`
- milestone-only `task.progress`

Anchor in the existing dispatch paths that currently send `TailEvent`.
Do not persist raw provider stream chunks.

Payload minimum:

```jsonc
{
  "task_id": "...",
  "provider": "claude",
  "model": "...",
  "effort": "...",
  "bro": "..."
}
```

For completion/failure, include elapsed and cost/error summary when available.

6.2 **Workflow lifecycle.**

Emit:

- `workflow.arc.started`
- `workflow.arc.completed`
- `workflow.arc.failed`
- `workflow.arc.cancelled`
- `workflow.arc.wait_registered`
- `workflow.arc.signal_received`
- milestone node start/completion events if volume remains acceptable

Anchor in `src/workflow/engine.rs`. Avoid making every internal `log_event`
durable; select the operationally meaningful subset.

6.3 **Whiteboard phase transition.**

Emit `whiteboard.phase_changed` from the same path that currently sends
`board-transitioned` routed signals. The system event is audit/egress; it does
not replace signal dispatch.

6.4 **Backpressure policy.**

If event append fails, the producer logs an error. For critical events such as
identity-required, return the error to the caller; for observation-only
task/arc events, do not fail the running task because journaling failed.

Document this per emit site in code comments.

**Deliverable:** real daemon activity appears in `system_event_list`, and
simple reactions can observe task/workflow/whiteboard lifecycle events.

**Tests:**

- dispatched dummy task emits started/completed
- tiny workflow emits arc started/completed
- Wait resolution emits signal-received
- whiteboard transition emits phase-changed and still dispatches existing
  routed signal

**Estimated size:** 500-900 lines Rust.

---

## Phase 7: Forgejo Identity Slice

**Prerequisites:** Phases 1-5.

**What gets built:** first end-to-end use case: distinct Forgejo principals for
bros.

7.1 **Identity registry tools.**

Add:

- `identity_list` - readonly
- `identity_get` - readonly

Optional ops-only tools:

- `identity_forget`
- `identity_verify`

Do not add destructive identity cleanup until there is a clear operational
need; external Forgejo user deletion is out of scope for v1.

7.2 **Identity lookup API.**

Add internal API:

```rust
async fn require_identity(
    state: Arc<SharedState>,
    scope: &str,
    instance: &str,
    subject: IdentitySubject,
) -> Result<Option<ExternalIdentity>>
```

Behavior:

- return existing valid mapping if present
- if missing, emit `bro.identity.required`
- return `Ok(None)` to indicate provisioning is pending

The `bro.identity.required` emit site belongs here, not Phase 6. Without the
identity registry there is no owner for dedupe, pending state, or mapping
lookup.

Do not block dispatch indefinitely waiting for provisioning in v1. Callers can
retry or route through a workflow that waits for `bro.identity.provisioned`.

7.3 **Forgejo ensure-user reaction.**

Create example reaction:

```text
examples/forgejo/reactions/ensure-bro-user.json
```

It subscribes to `bro.identity.required`, gates on
`identity_scope == "forgejo"`, and invokes `atom:forgejo-ensure-user@v1`.
Use the atom rather than direct `http_json` because the behavior is multi-step:
find/create user, create/verify token, store token secret, upsert identity
mapping, and emit `bro.identity.provisioned`.

7.4 **Identity mapping write.**

Provisioning writes the local identity registry inside
`atom:forgejo-ensure-user@v1`. Do not add a v1 reaction action
`identity_upsert`; that would widen the closed action catalog for one
Forgejo-specific path.

Do not make workflows write identity JSON by shell.

7.5 **Forgejo token handling.**

Token material is stored outside identity JSON. The identity record stores:

```text
token_ref = "secret:forgejo-bro-<safe-name>"
```

The provisioning atom creates or verifies the external token, then stores it
with `secrets::write_file_secret`. If the secret write fails, the atom fails
and the reaction retries or dead-letters through the normal outbox path. V1
does not store token material in identity JSON and does not require
operator-managed tokens for the automated Forgejo path.

7.6 **Keystone integration.**

Update the Keystone extension, not the base workflow, to request a Forgejo
identity for reviewer/implementer bros before PR/review actions.

Acceptance path:

```text
missing identity -> bro.identity.required -> reaction provisions -> mapping
exists -> Forgejo action uses mapped principal
```

If identity is pending, the workflow should either:

- wait on `bro.identity.provisioned`, or
- fail early with a clear blocked note and allow redispatch after provisioning.

Waiting is better for the demo.

7.7 **Self-approval smoke.**

Create a Forgejo smoke scenario:

1. implementer identity opens PR
2. reviewer identity reviews PR
3. Forgejo does not reject the review as self-approval
4. audit trail shows distinct Forgejo users
5. Blackbox identity mapping links each external user to bro/provider/model

**Deliverable:** a Keystone-style Forgejo flow can use distinct external
principals without hand-written provisioning hooks in every workflow.

**Tests:**

- identity miss emits event once per logical key
- repeated miss does not create duplicate outbox items when idempotency key
  matches
- identity upsert persists and reloads
- Forgejo unreachable retries, then dead-letters
- dead-letter retry succeeds after fake Forgejo recovers
- local Forgejo smoke verifies non-self review behavior

**Estimated size:** 500-1,000 lines Rust plus example artifacts/scripts.

---

## Phase 8: Surfaces, Docs, And Hardening

**Prerequisites:** Phases 1-7.

**What gets built:** operator quality, docs, examples, and policy hardening.

8.1 **Tool docs and surfaces.**

Update:

- `src/tool_docs.rs`
- `system-defaults/mcp-surfaces/routing.json`
- `docs/system-events.md`
- `docs/operating-blackbox.md` cross-link to the system-events runbook

Surface policy:

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

Add compile-time tool-doc coverage tests if the existing tool-doc test requires
every tool stanza.

8.2 **Examples.**

Add examples:

```text
examples/system-events/
  reactions/noop-task-completed.json
  reactions/forgejo-ensure-bro-user.json
  packets/forgejo-identity-required.json
  system-events-example.md
```

If the Forgejo work continues under `deploy/forgejo15/`, link the example
rather than duplicating bootstrap scripts.

8.3 **Runbook.**

Document operational loops:

- "reaction dead-lettered, why?"
- "event emitted but no reaction ran"
- "reaction ran twice"
- "identity missing for bro"
- "Forgejo token secret missing"

Each loop should name the MCP tools in order:

```text
system_event_open -> reaction_deliveries -> reaction_replay dry_run -> reaction_execute / reaction_retry
```

8.4 **Redaction audit.**

Add tests or fixture checks that stored event/outbox/replay output does not
include:

- Authorization headers
- Forgejo tokens
- `secret:` resolved values
- MCP server credentials

8.5 **Retention/compaction.**

Implement or verify:

- 10,000 event / 7 day event retention
- successful outbox compaction after 7 days
- all non-success records retained
- copy-forward temp file + fsync + rename
- `system_event_compact` ops tool returning journal/outbox compaction reports
- `system-defaults/maintenance` daily-compaction workflow + cron artifact that
  runs system-event compaction, edge storage GC, and vector compaction

Compaction tests must simulate an interrupted compaction by leaving a partial
temp file next to the current segment. On restore, the store must ignore the
partial temp file and preserve either the old complete segment or the new
complete segment; never merge partial data.

8.6 **Indexing decision.**

Do not index system events into the agentic corpus in v1. Revisit after
redaction and retention have production mileage.

8.7 **Roster integration decision.**

Do not change `/roster` in v1. Task lifecycle system events may later enrich
roster snapshots, but the first implementation keeps roster state on its
current task-store path and uses system events for journal/outbox/reaction
behavior.

**Deliverable:** operators can install, inspect, replay, retry, and debug
system-event reactions without reading daemon logs.

**Tests:**

- tool surface visibility tests
- docs/tool stanza coverage tests
- redaction tests
- compaction tests
- interrupted compaction restore test
- example dry-run commands

**Estimated size:** 300-700 lines Rust/docs/artifacts.

---

## Cross-Phase Risks

### JSONL Update Complexity

Appending JSONL is easy; updating outbox records is not. The simplest correct
v1 approach is whole-segment copy-forward under a lock. That is acceptable for
the expected low event volume. Optimize later only after measuring.

### Idempotency Is Mandatory

Every external side effect must have a stable idempotency key. If an action
cannot be made repeat-safe, it should not run from the outbox. This rule exists
because process crash can happen after the external effect and before local
success is recorded.

### Reaction Dependencies

Reactions to the same event are deterministic but not a dependency system.
If one reaction depends on another, emit a derived event and subscribe to that.
Do not encode dependency by reaction name ordering.

### HookOp Reuse

Reuse lower-level executor functions from workflow ops where possible, but do
not expose workflow hook schema as reaction schema. Workflow hooks carry
ArcContext semantics; reactions carry SystemEvent semantics.

### Secret Writes

The Forgejo identity slice depends on the Phase 1 `write_file_secret`
primitive. Do not write token material into identity JSON. If secret writing is
not implemented, Phase 7 is blocked rather than silently falling back to a
half-provisioned identity.

### Event Producer Failure Policy

Not every event append failure should fail the user operation. Journal append
failure for identity-required events is a functional dependency and should
surface as an error. Journal append failure for task lifecycle events is
observation and should log rather than fail the running task. Partial success
after journal append is reported through `EmitOutcome`, not a generic error.
Each emit site must state its failure policy.

## Final Acceptance Criteria

The implementation is ready when:

- synthetic `system_event_emit` creates a durable event and live broadcast
  delivery
- installed reactions dry-run with deterministic packet entity and rendered
  idempotency key
- outbox records survive restart and recover stale `claimed` states correctly
- side-effecting actions cannot run without idempotency keys
- retries and dead letters are visible through `reaction_deliveries`
- dead letters project to `bbox_note(kind="blocked")`
- real task/workflow/whiteboard events are emitted without persisting raw
  provider stream chunks
- Forgejo per-bro identity provisioning persists mappings and avoids duplicate
  external users
- a Forgejo self-approval smoke shows distinct bro identities in the audit
  trail
- MCP surface restrictions match the design doc
- `rtk cargo test --bin blackboxd` passes

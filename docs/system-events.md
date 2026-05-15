# System events

System events are blackbox's typed journal of "something happened" — task
lifecycle transitions, workflow arc node changes, identity provisioning
requests, coordination signals from external systems. Producers emit events
through `EventHub`; reactions subscribe by event kind, optionally gate on
payload, and dispatch durable actions via the outbox.

This page is the operator-facing reference. For the implementation tour see
`src/system_events/` (hub, store, outbox, reactions, worker, identity,
forgejo, gate, executors, template).

## Concepts

| Concept | What it is |
|---|---|
| **Event** | Immutable typed record. Has `kind`, `producer`, optional `project`/`principal`/`subject`, `correlation`, optional `causation_id`, and an opaque `payload`. Identified by `evt-<uuid>`. |
| **Producer** | Daemon code path that emits the event. Examples: `orchestration.dispatch`, `workflow.engine`, `forgejo.webhook`. Never an LLM. |
| **Principal** | Attribution: who was acting when the event was produced. `kind` plus optional `bro`/`provider`/`model`/`effort`. Carries no token material. |
| **Subject** | What the event is about. E.g. `{kind: "bro", id: "bro:keystone-review"}` for an identity-required event. |
| **Reaction** | Operator-installed JSON spec subscribing to one or more event kinds. Fires an action through the outbox. Identified by `name`. |
| **Outbox record** | Durable per-(event, reaction) delivery row. Status lifecycle: `pending → claimed → succeeded \| retry_at \| dead_lettered`. |
| **Idempotency key** | Reaction-rendered string. Prevents duplicate delivery when reactions retry against the same event. Required for non-`emit_event` actions. |
| **Gate** | Optional `domain:...` packet ref on a reaction. Routes by payload-shape; lets reactions opt out without emitting on the wire. |
| **Identity** | Durable `(scope, instance, subject, provider, model)` → `ExternalIdentity` mapping. Holds `token_ref` (`secret:<name>`), never the secret value. |

## Surfaces

| Tool | Minimum surface | Notes |
|---|---|---|
| `system_event_list` | `readonly` | Filter by kind/producer/project. |
| `system_event_open` | `readonly` | Returns event + causation chain. |
| `reaction_list` | `default` | All installed reactions. |
| `reaction_deliveries` | `default` | Outbox rows. Filter by event_id/status. |
| `reaction_replay` (`mode=dry_run`) | `default` | Renders idempotency key, gate verdict, and redacted action args. No side effects. |
| `identity_list` / `identity_get` | `readonly` | Mappings only — no token material. |
| `system_event_emit` | `ops` | Synthetic injection. Accepts kind, producer, project, causation_id, principal, subject, correlation, payload. Use for ad-hoc replay/debugging — production identity flow uses `require_identity`. |
| `reaction_install` | `ops` | Installs/updates spec. |
| `reaction_execute` | `ops` | Creates an audited outbox row and executes one reaction against one event. `force=true` bypasses succeeded-idempotency suppression only. |
| `reaction_retry` | `ops` | Requeues a dead-lettered outbox record by id. |

The agent-internal surface (dispatched bros) disallows `system_event_emit`,
`reaction_install`, and `reaction_retry` as a backstop alongside the
dispatch-time mechanical recursion guard.

## Event/reaction/outbox flow

```text
producer code
   │
   ▼
EventHub.emit(draft)
   │   ├── EventStore.append(envelope)   (current.jsonl + fsync)
   │   ├── reactions.match(event)
   │   │      └── for each enabled reaction with matching kind:
   │   │            ├── render idempotency_key
   │   │            ├── evaluate gate packet (if `when:` set)
   │   │            └── OutboxStore.create_record(...)
   │   └── tx.send(event)                 (broadcast to subscribers)
   ▼
worker loop
   │
   ▼
OutboxStore.claim_next(now, process_id)
   │   ▶ executor.run(reaction, event, record)
   │      ├── render action args (templating + secret resolution)
   │      ├── execute (http_json | mcp_call | atom_invoke | start_workflow | emit_event)
   │      └── ActionOutcome { status, response_summary }
   ▼
status transitions:
   Succeeded         → mark_succeeded(id, summary)        — compacted after 7d
   RetryableFailure  → mark_retry_at(id, next, redacted)  — backoff schedule
   PermanentFailure  → mark_dead_lettered(id, reason)     — sticky, retained
   SkippedByGate     → mark_succeeded(id, "skipped")
```

## Identity flow

The Phase 7 acceptance path is:

```text
1. workflow.RequestIdentity.on_enter:
     require_identity { scope, instance, bro, provider, model, ... }
        ↓
2. EventHub.require_identity:
     - identity_registry.get(scope, instance, subject, provider, model)
        - found → return ExternalIdentity
        - missing AND not already pending →
            emit bro.identity.required (event + outbox + dead-letter on failure)
            mark_pending(...)
            return None
        - missing AND pending → return None  (dedup)
        ↓
3. Reaction forgejo-ensure-bro-user fires:
     atom_invoke forgejo-ensure-user
        → creates Forgejo user
        → writes secret token to $XDG_DATA_HOME/blackbox/secrets/<name>
        → identity_registry.upsert(ExternalIdentity{ token_ref: "secret:<name>" })
        → emits bro.identity.provisioned
        ↓
4. workflow.AwaitIdentity.wait { signal: bro.identity.provisioned }
        ↓
5. workflow loops back to RequestIdentity.on_enter:
     identity now mapped → returns ExternalIdentity
        ↓
6. workflow.FetchDiff → Review (ensemble) → Aggregate (executor):
     reviewer ensemble reads ${vars.pr_diff}; aggregator produces strict
     JSON {event, body, action} into ${Aggregate.output}.
        ↓
7. workflow.PostReview.on_enter:
     parse_json from ${Aggregate.output} into vars.review_payload, then
     http_json POST /pulls/.../reviews and (when gated) /merge — each
     call with secret_headers: { Authorization:
       "token ${vars.identity_result.identity.token_ref}" }
        → secret_headers resolves "secret:<name>" → real token at request time
        → token never enters vars, logs, journal, outbox, replay output
```

## Redaction

Secret material never lives in event payloads, outbox records, or replay
output. Two layers enforce this:

1. **`gate::redact_values`** runs on the rendered action args before they
   land in `OutboxRecord.response_summary` or `reaction_replay` dry-run
   output. It replaces any `Authorization` header value and any string
   containing `secret:` with `[REDACTED]`.
2. **`worker::redact_string`** runs on all dead-letter reasons and retry
   error strings before they hit `OutboxStore` or `bbox_note(blocked)`.
   It strips `secret:<name>` references and `Bearer <value>` tokens.

Operators who add new error paths or response summaries should route the
string through `redact_string` and the JSON through `redact_values`.

## Retention and compaction

| Store | Trigger | Policy |
|---|---|---|
| **EventStore** (`events/journal/current.jsonl`) | `compact_with_now(now)` / `system_event_compact` | Keep newest 10,000 events; drop any event with `occurred_at` older than 7 days. |
| **OutboxStore** (`events/outbox/current.jsonl`) | `compact_with_now(now)` / `system_event_compact` | Drop `succeeded` records older than 7 days. All other statuses (`pending`, `claimed`, `retry_at`, `dead_lettered`) are retained. |

Compaction uses copy-forward: writes `current.tmp`, fsyncs, then atomically
renames to `current.jsonl`. An interrupted compaction (process killed mid-write)
leaves the partial `current.tmp` next to the old `current.jsonl`; the next
load reads `current.jsonl` only and the orphaned tmp is removed on the next
store reopen or compaction pass. Either the old complete segment or the
new complete segment is preserved — partial data is never merged.

Compaction runs once at daemon startup, after reaction restore and outbox
stale-claim recovery, before the outbox worker spawns. Operators can also run
`system_event_compact` from the ops surface.

For long-lived daemons, install the bundled daily maintenance flow:

```text
bbox_artifact_install(kind="workflow", source="system-defaults/maintenance/workflows/daily-compaction-arc.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/embed/compaction-policy.json")
bbox_artifact_install(kind="packet", source="system-defaults/maintenance/packets/cron-routing/daily-compaction.json")
bbox_artifact_install(kind="cron", source="system-defaults/maintenance/crons/daily-compaction.json")
```

That cron starts `daily-compaction-arc`, which runs system-event compaction,
edge sidecar storage GC, and vector partition compaction under the existing
embed compaction policy.

## Operational loops

Each loop uses the same MCP tool order:

```text
system_event_open  →  reaction_deliveries  →  reaction_replay (dry_run)  →  reaction_execute / reaction_retry
```

### Reaction dead-lettered, why?

1. `reaction_deliveries(status="dead_lettered")` to find the outbox row.
2. `system_event_open(event_id=...)` to read the event and any causation
   chain.
3. Inspect `dead_letter_reason` and `last_error` on the outbox record — both
   are redacted; if they reference a missing secret, see "Forgejo token
   secret missing" below.
4. `reaction_replay(mode="dry_run", event_id=..., reaction=...)` to confirm
   the gate verdict and rendered action args. Check that templating
   resolved cleanly and the gate routed as expected.
5. Fix the underlying issue (missing secret, broken atom, wrong template),
   then `reaction_retry(outbox_id=...)` for an existing dead letter or
   `reaction_execute(event_id=..., reaction=...)` for an audited manual replay.

### Event emitted but no reaction ran

1. `system_event_open(event_id=...)` — confirm the event was journaled and
   note its `kind`.
2. `reaction_list()` — confirm a reaction subscribes to that kind and is
   `enabled: true`. A reaction with `enabled: false` will not match.
3. `reaction_deliveries(event_id=...)` — expect a row per matching
   reaction. If zero rows:
   - kind mismatch (typo, new kind not yet registered),
   - reaction disabled,
   - or reaction-load warning (see daemon logs).
4. If a row exists but is `pending` and never advances, check daemon worker
   logs.
5. If gate skipped the reaction, the row shows `succeeded` with summary
   `{"skipped_by_gate": "..."}`. Confirm with `reaction_replay dry_run`.

### Reaction ran twice

1. `reaction_deliveries(event_id=...)` — expect at most one row per
   reaction. Multiple rows for the same (event_id, reaction) indicate
   missing idempotency key on the reaction spec.
2. Check the reaction spec via `reaction_list()` — the `idempotency_key`
   template should produce a unique-per-effect string. Examples:
   - `forgejo:${event.payload.instance}:${event.subject.id}` for per-bro
     Forgejo provisioning,
   - `pr-comment:${event.payload.pr_id}:${event.payload.review_id}` for
     review posting.
3. If duplicate idempotency keys exist, that is the dedup target. If the
   key is genuinely unique per effect but the action itself fired twice,
   the action endpoint is non-idempotent — fix it upstream.
4. `reaction_replay(mode="dry_run")` to confirm rendered key.
5. If a second execution is intentional after inspection,
   `reaction_execute(event_id=..., reaction=..., force=true)` bypasses only
   succeeded-idempotency suppression. Gates, causation guards, redaction, and
   outbox audit still apply.

### Identity missing for bro

The workflow logs `identity_result.status = "pending"` and branches into
`AwaitIdentity`. If the await never wakes:

1. `identity_list()` — confirm no mapping exists for
   `(scope, instance, subject=bro:<name>, provider, model)`.
2. `system_event_list(kind="bro.identity.required")` and find the matching
   event — the `subject.id` is `bro:<name>`.
3. `reaction_deliveries(event_id=...)` — find the provisioning reaction
   (e.g. `forgejo-ensure-bro-user`). Inspect status.
4. If dead-lettered, follow the dead-letter loop above. The most common
   cause is a missing admin token secret on the daemon host.
5. `reaction_replay(mode="dry_run", event_id=..., reaction=...)` to confirm
   the atom invocation args render correctly.
6. After fixing, `reaction_retry(outbox_id=...)`. The reaction emits
   `bro.identity.provisioned` on success; the `AwaitIdentity` wait
   resumes; the workflow loops back to `RequestIdentity` and proceeds.

### Forgejo token secret missing

`http_json` with `secret_headers` resolves `secret:<name>` at request time
via `blackbox::secrets::resolve`. Failure modes:

1. **Secret file absent.** The daemon log shows
   `secret_headers.Authorization: secret '<name>' could not be resolved`.
   The outbox record dead-letters with a redacted reason. Confirm:
   - systemd `LoadCredential=<name>:<path>` drop-in, OR
   - `$XDG_DATA_HOME/blackbox/secrets/<name>` exists and is readable, OR
   - `BLACKBOX_SECRET_<UPPER_SNAKE_NAME>` env var on the daemon.
2. **Wrong token_ref.** `identity_get(...)` to inspect the mapping — the
   `token_ref` field must be `"secret:<name>"`, never raw. If it's wrong,
   the provisioning reaction stored a bad ref; investigate the
   provisioning atom.
3. **Provisioning never wrote the secret.** Check the dead-letter loop on
   the provisioning reaction.
4. After fixing the secret on disk, `reaction_retry(outbox_id=...)` on the
   failed delivery. The token resolves on the next attempt.

## Examples

Operator-ready examples live in
[System Events Example](../examples/system-events/system-events-example.md).
They are wired to compile against the installed reaction and packet contracts
but are intentionally generic — adapt to the host's Forgejo instance, secret
names, and atom identifiers.

| File | Purpose |
|---|---|
| `reactions/noop-task-completed.json` | Minimal reaction template subscribing to `task.completed`. Uses `emit_event` (no idempotency key required) to forward a sanitized summary. Good for verifying the wiring without external side effects. |
| `reactions/forgejo-ensure-bro-user.json` | Copy of the Phase 7 reference reaction. Subscribes to `bro.identity.required`, gates on `payload.identity_scope == "forgejo"`, invokes the `forgejo-ensure-user` atom. |
| `packets/forgejo-identity-required.json` | Gate packet referenced by the Forgejo reaction. Routes by `payload.identity_scope`. |

## Indexing and roster

System events are **not** indexed into the agentic corpus in v1. Revisit
after redaction and retention have production mileage.

`/roster` snapshots continue to come from the task-store path. Task
lifecycle events may enrich roster output later; the v1 implementation
leaves roster on its existing surface and uses system events for
journal/outbox/reaction behavior only.

## Cross-link

For day-2 operational checks (search health, embedding queue, reindex,
compaction of the transcript index), see
[Operating blackbox — day 2 runbook](operating-blackbox.md). The
system-events stores have their own compaction described above and are
not driven by the transcript-side `bbox_*` reindex tools.

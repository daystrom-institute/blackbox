# System-events examples

Operator-ready reaction and packet artifacts demonstrating the v1
system-events surface. Read [`docs/system-events.md`](../../docs/system-events.md)
for the runbook these examples target.

## Layout

```text
examples/system-events/
├── reactions/
│   ├── noop-task-completed.json        — minimal wiring smoke test
│   └── forgejo-ensure-bro-user.json    — Phase 7 identity provisioning
└── packets/
    └── forgejo-identity-required.json  — gate consumed by the Forgejo reaction
```

The Forgejo artifacts mirror their counterparts under
`examples/forgejo/reactions/` and `examples/forgejo/packets/`. They are
duplicated here so this directory is a self-contained walkthrough of the
system-events flow without requiring the full Keystone setup. Operators
running the Keystone arc can keep using the originals; nothing here
overrides them.

## Inspect with the MCP surface

These examples are JSON-only artifacts — they do not need a daemon to
read. To wire them into a running blackbox, use the operator surface:

```text
# Install the gate packet (operator surface).
bbox_compile(packet=<contents of forgejo-identity-required.json>)

# Install a reaction (operator surface). Use replace=true to overwrite.
reaction_install(spec=<contents of forgejo-ensure-bro-user.json>)

# Confirm.
reaction_list()
```

To preview what a reaction would do without executing the action, use
`reaction_replay` in `dry_run` mode — the default surface allows this.

`system_event_emit` is the ops-only synthetic-injection surface. It
accepts `kind`, `producer`, optional `project`, optional `causation_id`,
optional `principal`, optional `subject`, optional `correlation`, and
`payload`. In production, identity events come from
`EventHub::require_identity` (driven by the `require_identity` workflow
hook op) — `system_event_emit` is for ad-hoc replay and operator
debugging, not the normal path.

For a dry-run of the Forgejo identity reaction, supply the same
`principal`/`subject`/`payload` shape the production producer would
emit. The reaction's `idempotency_key` template
(`forgejo:${event.payload.instance}:${event.subject.id}:${event.principal.provider}:${event.principal.model}`)
references the typed attribution fields directly, so they must be set
for the key to render cleanly:

```text
system_event_emit(
  kind="bro.identity.required",
  producer="manual-test",
  principal={
    "kind":"bro",
    "bro":"keystone-review",
    "provider":"claude",
    "model":"claude-haiku-4-5-20251001"
  },
  subject={"kind":"bro","id":"bro:keystone-review"},
  payload={
    "identity_scope":"forgejo",
    "instance":"local-forgejo15",
    "bro":"keystone-review",
    "provider":"claude",
    "model":"claude-haiku-4-5-20251001",
    "username":"bro-keystone-review-claude-claud-09d0ce",
    "display_name":"keystone-review / claude claude-haiku-4-5-20251001",
    "email":"bro-keystone-review@blackbox.local"
  }
)

# The event_id is returned by the emit. Replay against it:
reaction_replay(mode="dry_run",
  event_id="evt-...",
  reaction="forgejo-ensure-bro-user")
```

`principal.model` is the exact catalog model ID — the same string the
brofile and `require_identity` use. `payload.username` is the bounded
projection produced by `IdentityRequest::username()` (39-char cap with
a 6-hex `sha256(model)` suffix when the readable stem would exceed
Forgejo's 40-char username limit). Short historical model names like
`haiku-4.5` still produce the legacy readable shape
`bro-<bro>-<provider>-<modelslug>` verbatim.

`reaction_replay` returns the rendered idempotency key, the gate verdict,
and the rendered action args with all `secret:` references and
`Authorization` header values replaced by `[REDACTED]`. The dry-run
output never reaches the outbox or fires the underlying atom.

## What each artifact does

### `reactions/noop-task-completed.json`

Subscribes to `task.completed`. Action is `emit_event`, which does not
require an idempotency key, so the reaction is a strict no-op forwarder
that re-emits a sanitized echo event. Good for confirming the
reaction loop is wired without external side effects.

### `reactions/forgejo-ensure-bro-user.json`

The Phase 7 identity-required reaction. Subscribes to
`bro.identity.required`; gates on `payload.identity_scope == "forgejo"`;
invokes the `forgejo-ensure-user` atom which creates the Forgejo user,
stores its token to `$XDG_DATA_HOME/blackbox/secrets/<name>`, and
emits `bro.identity.provisioned` on success.

Adapt these fields for your host:

- `forgejo-admin-token` — secret name on the daemon host with admin API
  access. Either via systemd `LoadCredential` or via the
  `$XDG_DATA_HOME/blackbox/secrets/` directory.
- `FORGEJO_BASE_URL` — daemon env var pointing at your Forgejo instance.
- `token_secret_name` template — per-bro secret name pattern. The default
  produces `forgejo-bro-<derived-username>` which is referenced from the
  workflow's `secret_headers` as `secret:forgejo-bro-<username>`.

### `packets/forgejo-identity-required.json`

Gate packet referenced by the Forgejo reaction's `when` field. Routes by
`payload.identity_scope`:

- `payload.identity_scope == "forgejo"` → `allow`, reaction fires.
- everything else → `skip`, reaction marks outbox as `succeeded` with
  summary `{"skipped_by_gate": "..."}`.

The packet is `scope: global` because the same identity domain applies
across every project on the daemon.

## Phase 7 acceptance path

End-to-end, with the workflow under
`examples/keystone/workflows/reviewer-arc-with-identity.json`:

1. The reviewer workflow's `RequestIdentity` node calls
   `require_identity` (`scope=forgejo, instance=local-forgejo15, ...`).
2. The Forgejo identity is unmapped → `EventHub` emits
   `bro.identity.required`, marks the (scope, instance, subject) pending.
3. The reaction here fires: `forgejo-ensure-user` creates the user, writes
   the token secret, calls `identity_registry.upsert(...)`, and emits
   `bro.identity.provisioned`.
4. The workflow's `AwaitIdentity.wait` resumes on the provisioned signal,
   loops back to `RequestIdentity`, and now resolves to a mapped identity.
5. The workflow then runs `FetchDiff` (GET `.../pulls/{n}.diff` →
   `vars.pr_diff`), the `Review` ensemble reads `${vars.pr_diff}`, the
   `Aggregate` executor produces the strict JSON
   `{event, body, action}` into `${Aggregate.output}`.
6. `PostReview.on_enter` runs `parse_json` from `${Aggregate.output}`
   into `vars.review_payload`, then `http_json` POSTs the review and
   (when gated by `domain:hook-when/should-merge`) the merge. Every
   `http_json` call uses `secret_headers` with the value
   `token ${vars.identity_result.identity.token_ref}` — `secret:<name>`
   resolves against the on-disk secret at request time. The raw token
   never enters vars, outbox rows, replay output, or daemon logs.

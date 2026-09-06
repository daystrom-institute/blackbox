# Ingress Paths

Ingress is how outside events reach workflow state.

Webhooks, pollers, and crons all do the same thing once they enter the daemon:
produce an entity, extract the fields the packet cares about, apply a routing
packet, then dispatch the verdict. The verdict starts an arc, signals a waiting
arc, cancels an arc, dead-letters the event, or ignores it.

```
┌──────────┐  ┌──────────┐  ┌──────────┐
│ Webhooks │  │ Pollers  │  │  Crons   │
└────┬─────┘  └────┬─────┘  └────┬─────┘
     │              │              │
     ▼              ▼              ▼
  Extractor ──► Routing Packet ──► Dispatch
                    │
        ┌───────────┼───────────┐
        ▼           ▼           ▼
   start_arc   signal_arc   cancel_arc
   ignore      dead_letter
```

The daemon has no hardcoded Forgejo/GitHub/Slack path. JSON-speaking upstreams
all converge on extractor to packet to dispatch. You iterate on routing by
replaying payloads, not by redeploying code.

The webhook, poller, and cron list tools return name-ordered summary pages
(default 20, maximum 100). Continue with `next_offset` as `offset`, or select an
exact `name`. `detail=true` adds safe configuration diagnostics. List replies
omit credentials, URL paths/query/userinfo, payload values, selector constants,
and host project paths, including in detail mode.

## Webhooks

The daemon exposes `POST /webhook/<name>` for inbound webhook deliveries.
Each webhook carries a signature scheme, an extractor, and a routing
packet.

### Install a webhook

```json
bro_webhook_install(spec = {
  "name": "forgejo-pr",
  "signature": {
    "scheme": "hmac_sha256",
    "header": "X-Forgejo-Signature",
    "secret_env": "FORGEJO_WEBHOOK_SECRET"
  },
  "extractor": {
    "event_type": "$._headers.x-gitea-event",
    "action": "$.action",
    "repo": "$.repository.full_name",
    "pr_number": "$.number",
    "pr_url": "$.pull_request.html_url",
    "pr_title": "$.pull_request.title",
    "sender": "$.sender.login"
  },
  "routing_packet": "packet-7f01324e"
})
```

### List and inspect

```json
bro_webhook_list()
bro_webhook_deliveries(name = "forgejo-pr", since = "2026-05-01T00:00:00Z")
```

### Replay for iteration

The replay endpoint lets you test routing rules without pushing real
events. Skips signature verification so you can iterate on packet rules.

```json
bro_webhook_replay(
  name = "forgejo-pr",
  body = { "action": "opened", "pull_request": { ... } },
  headers = { "x-gitea-event": "pull_request" }
)
```

Replays are recorded in the delivery ring buffer (`source: "replay"`) so
they appear in `bro_webhook_deliveries`.

### Choosing a signature scheme

| Scheme | When to use |
|---|---|
| `hmac_sha256` | Forgejo, GitHub, most code hosts |
| `none` | Internal sidecars (bro-slack), replay-only webhooks |

## Pollers

Pollers are interval-driven HTTP fetchers. Use when the upstream doesn't
push (no webhook capability) or the daemon has no public ingress to
receive webhooks.

A poller fetches a URL on a schedule, optionally explodes the response
into N events via an `iterate` selector, extracts entity fields per
event, deduplicates by a stable id path, and routes each event through
the same routing packet pipeline.

### Install a poller

```json
bro_poller_install(spec = {
  "name": "github-issues",
  "every_seconds": 300,
  "source": {
    "url": "https://api.github.com/repos/owner/repo/issues?state=open",
    "method": "GET",
    "headers": {
      "Authorization": "Bearer ...",
      "Accept": "application/vnd.github.v3+json"
    }
  },
  "iterate": "$",            // JSONPath: explode array into per-item events
  "extractor": {
    "issue_number": "$.number",
    "title": "$.title",
    "state": "$.state",
    "labels": "$.labels[*].name",
    "assignee": "$.assignee.login"
  },
  "dedup_id_path": "$.number",
  "routing_packet": "packet-issue-triage"
})
```

### Key parameters

| Field | Purpose |
|---|---|
| `every_seconds` | Poll interval (≥ `BBOX_POLLER_MIN_INTERVAL_SECS`, default 5) |
| `iterate` | JSONPath selector to explode array responses into per-item events |
| `dedup_id_path` | Stable id for dedup (in-memory recent-seen ring per poller) |
| `extractor` | Field projections - same shape as webhook extractors |
| `routing_packet` | Packet that classifies each extracted entity |

### Management

```json
bro_poller_list()
// Re-install with the same name to replace the running tick loop
bro_poller_install(spec = { ... })  // same name = replace
```

## Crons

Crons are calendar-driven triggers - no fetch, just a scheduled tick that
produces a synthetic entity and routes it through the packet pipeline.

### Install a cron

```json
bro_cron_install(spec = {
  "name": "daily-triage",
  "schedule": "0 0 9 * * *",   // 6-field: sec min hour dom mon dow
  "payload": {
    "scope": "global",
    "window_days": 1
  },
  "routing_packet": "packet-daily-triage"
})
```

At each tick, the daemon merges two synthetic fields into the entity:

- `cron_name` - the cron's name (so routing rules can discriminate)
- `tick_at` - ISO 8601 timestamp of the tick

Your routing packet can use `Eq{field: "cron_name", value: "daily-triage"}`
to route only this cron's ticks.

### Multiple crons, one routing packet

Use the `cron_name` field to discriminate:

```json
// Routing rule in the packet:
{
  "id": "route_daily_triage",
  "antecedent": {"op": "Eq", "field": "cron_name", "value": "daily-triage"},
  "consequent": {"route": "start_arc", "workflow": "daily-triage"}
}
```

### Concurrency and scheduling

| Parameter | Default | Purpose |
|---|---|---|
| `schedule` | required | 6-field cron expr (`sec min hour dom mon dow`) |
| `concurrency_cap` | `1` | Max concurrent runs (set `0` to disable) |
| `routing_packet` | required | Packet id for classification |
| `default_project_dir` | optional | Default project for `start_arc` verdicts |

### Preview upcoming ticks

```json
bro_cron_upcoming(schedule = "0 0 9 * * *", count = 5)
```

### Management

```json
bro_cron_list()
```

## Debugging routing rules

The universal debugging pattern across all three ingress types:

1. **Replay** - `bro_webhook_replay()` for webhooks; pollers and crons
   can be triggered manually by calling their dispatch path
2. **Inspect deliveries** - `bro_webhook_deliveries()` shows the
   classification verdict for every webhook event (and replay)
3. **Inspect signals** - `bro_signals()` shows whether `signal_arc`
   verdicts matched waiting arcs or fell idle
4. **Check arc state** - `bro_arc_status()` shows whether a
   `start_arc` verdict actually started running

## When to use which

| Trigger | Use when |
|---|---|
| **Webhook** | Upstream pushes events (code hosts, Slack sidecar) |
| **Poller** | Upstream has a REST API but no webhooks; daemon has egress to the source |
| **Cron** | Time-based trigger with no external data source (daily triage, periodic cleanup) |

All three share the same extractor + routing packet + dispatch pipeline.
Once you've written a routing packet for one trigger type, it works for
any of them.

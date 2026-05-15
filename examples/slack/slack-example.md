# bro-slack v1 — Slack sidecar bridge for blackbox

End-to-end example of the Slack transport adapter pattern. The `bro-slack`
binary owns the Socket Mode WebSocket connection; the daemon routes
events through packets and workflows. The daemon never links a Slack crate.

## Layout

```
examples/slack/
├── webhooks/
│   └── slack.json              # extractor + signature + routing packet ref (§6.5)
├── packets/
│   └── routing-slack.json      # webhook event → routing verdict (§7.2)
├── workflows/
│   ├── slack-badgey-ask.json   # dispatch-capable app mention handler
│   ├── slack-badgey-readonly.json # read-only mention / anonymous slash handler
│   └── slack-bbox-command.json # /bbox slash command dispatch handler
├── scripts/
│   └── install.sh              # compile + install artifacts via /admin/* endpoints
├── manifest.yaml               # Slack app manifest template (v1 scopes)
├── slack-example.md            # this file

deploy/
└── bro-slack.service           # systemd unit template
```

## Prerequisites

- `blackboxd` running (default `127.0.0.1:7264`)
- `bro-slack` binary compiled (`cargo build --release --bin bro-slack`)
- A Slack app with Socket Mode enabled and the following scopes granted:
  - `connections:write` (app-level token for Socket Mode)
  - `app_mentions:read`, `chat:write`, `commands`, `reactions:read`, `users:read`
  - `channels:history` (or `groups:history` / `im:history` as needed)
- Environment variables for the sidecar:
  - `SLACK_APP_TOKEN` (xapp-1-*)
  - `SLACK_SIGNING_SECRET` (optional for Events API; unused on Socket Mode path)
- Environment variables for the daemon:
  - `SLACK_BOT_TOKEN` (xoxb-*) for outbound `http_json` to `slack.com/api/*`
- Optional: `BRO_SLACK_SHARED_SECRET` for HMAC on the sidecar→daemon hop (both processes)
- `jq`, `curl`

## Quick start

### 1. Install artifacts into the daemon

```sh
cd examples/slack
./scripts/install.sh
```

This compiles the routing packet, installs the three workflow specs,
and installs the webhook spec. All are idempotent (re-run safe).

### 2. Configure identities

Create `~/.bro/slack-identities.json`:

```jsonc
{
  "T01234567": {
    "U01ABC": { "bbox_user": "alice", "scopes": ["all"] },
    "U02DEF": { "bbox_user": "bob",   "scopes": ["read"] }
  }
}
```

Users with `"all"` scope can dispatch; `"read"` users get the read-only
workflow. Unmapped users are `"anonymous"` with read-only access.

### 3. Run the sidecar

```sh
bro-slack \
  --self-user-id U0BOTUSR0 \
  --self-bot-id  B0BOTBOT0 \
  --log-level debug
```

The sidecar opens a Socket Mode connection and forwards normalized
events to `http://127.0.0.1:7264/webhook/slack`.

### 4. Interact in Slack

- `@bot ask about project X` → badgey-ask (dispatch) or badgey-readonly (read-only)
- `/bbox inbox` → inbox summary
- `/bbox status [arc_id]` → arc status
- React with `:white_check_mark:` on a bot-posted proposal → resumes the arc
- Click "Apply" button on a Block Kit proposal → resumes the arc with `proposal_id`

## Replay — iterate on routing rules without firing arcs

The daemon provides `POST /webhook/:name/replay` for testing routing
rules against synthetic payloads. Useful when iterating on the routing
packet or debugging "why didn't my event route?"

### App mention (dispatch-capable user)

```sh
curl -s -X POST http://127.0.0.1:7264/webhook/slack/replay \
  -H 'Content-Type: application/json' \
  -H 'X-Slack-Envelope-Id: replay-001' \
  -d '{
    "_meta": {
      "source": "bro-slack",
      "workspace_id": "T01",
      "self_bot_id": "Bbot",
      "self_user_id": "Ubot",
      "received_at": "2026-05-05T12:34:56.789Z",
      "envelope_id": "replay-001",
      "retry_attempt": 0,
      "bbox_user": "alice",
      "bbox_scopes": ["all"],
      "bbox_can_dispatch": true
    },
    "_headers": {
      "x-slack-envelope-id": "replay-001"
    },
    "type": "events_api",
    "event_type": "app_mention",
    "team_id": "T01",
    "channel": "C01",
    "channel_type": "channel",
    "user": "Ualice",
    "ts": "1730816096.000300",
    "thread_ts": "1730816060.000100",
    "text": "<@Ubot> can you triage #ops",
    "subtype": null,
    "bot_id": null,
    "reaction": null,
    "item_ts": null,
    "command": null,
    "command_text": null,
    "response_url": null,
    "trigger_id": null,
    "action_id": null,
    "action_value": null,
    "view_id": null,
    "view_state_values": null,
    "files": [],
    "raw": {
      "event": {
        "type": "app_mention",
        "user": "Ualice",
        "text": "<@Ubot> can you triage #ops",
        "ts": "1730816096.000300",
        "thread_ts": "1730816060.000100",
        "channel": "C01"
      }
    }
  }' | jq
```

Expect: `verdict_classification: "start_arc"`, `workflow: "slack-badgey-ask"`.

### App mention (anonymous / read-only user)

```sh
curl -s -X POST http://127.0.0.1:7264/webhook/slack/replay \
  -H 'Content-Type: application/json' \
  -H 'X-Slack-Envelope-Id: replay-002' \
  -d '{
    "_meta": {
      "source": "bro-slack",
      "workspace_id": "T01",
      "self_bot_id": "Bbot",
      "self_user_id": "Ubot",
      "received_at": "2026-05-05T12:34:56.789Z",
      "envelope_id": "replay-002",
      "retry_attempt": 0,
      "bbox_user": "anonymous",
      "bbox_scopes": ["read"],
      "bbox_can_dispatch": false
    },
    "_headers": { "x-slack-envelope-id": "replay-002" },
    "type": "events_api",
    "event_type": "app_mention",
    "team_id": "T01",
    "channel": "C01",
    "channel_type": "channel",
    "user": "Uunknown",
    "ts": "1730816096.000300",
    "thread_ts": null,
    "text": "<@Ubot> what is the project status?",
    "subtype": null,
    "bot_id": null,
    "reaction": null,
    "item_ts": null,
    "command": null,
    "command_text": null,
    "response_url": null,
    "trigger_id": null,
    "action_id": null,
    "action_value": null,
    "view_id": null,
    "view_state_values": null,
    "files": [],
    "raw": {
      "event": {
        "type": "app_mention",
        "user": "Uunknown",
        "text": "<@Ubot> what is the project status?",
        "ts": "1730816096.000300",
        "channel": "C01"
      }
    }
  }' | jq
```

Expect: `verdict_classification: "start_arc"`, `workflow: "slack-badgey-readonly"`.

### Reaction added (white_check_mark)

```sh
curl -s -X POST http://127.0.0.1:7264/webhook/slack/replay \
  -H 'Content-Type: application/json' \
  -H 'X-Slack-Envelope-Id: replay-003' \
  -d '{
    "_meta": {
      "source": "bro-slack",
      "workspace_id": "T01",
      "self_bot_id": "Bbot",
      "self_user_id": "Ubot",
      "received_at": "2026-05-05T12:34:56.789Z",
      "envelope_id": "replay-003",
      "retry_attempt": 0,
      "bbox_user": "alice",
      "bbox_scopes": ["all"],
      "bbox_can_dispatch": true
    },
    "_headers": { "x-slack-envelope-id": "replay-003" },
    "type": "events_api",
    "event_type": "reaction_added",
    "team_id": "T01",
    "channel": "C01",
    "channel_type": "channel",
    "user": "Ualice",
    "ts": null,
    "thread_ts": null,
    "text": null,
    "subtype": null,
    "bot_id": null,
    "reaction": "white_check_mark",
    "item_ts": "1730816096.000300",
    "command": null,
    "command_text": null,
    "response_url": null,
    "trigger_id": null,
    "action_id": null,
    "action_value": null,
    "view_id": null,
    "view_state_values": null,
    "files": [],
    "raw": {
      "event": {
        "type": "reaction_added",
        "user": "Ualice",
        "reaction": "white_check_mark",
        "item": { "channel": "C01", "ts": "1730816096.000300" }
      }
    }
  }' | jq
```

Expect: `verdict_classification: "signal_arc"`, `signal: "proposal-approved"`.

### Block Kit action (apply_proposal)

```sh
curl -s -X POST http://127.0.0.1:7264/webhook/slack/replay \
  -H 'Content-Type: application/json' \
  -H 'X-Slack-Envelope-Id: replay-004' \
  -d '{
    "_meta": {
      "source": "bro-slack",
      "workspace_id": "T01",
      "self_bot_id": "Bbot",
      "self_user_id": "Ubot",
      "received_at": "2026-05-05T12:34:56.789Z",
      "envelope_id": "replay-004",
      "retry_attempt": 0,
      "bbox_user": "alice",
      "bbox_scopes": ["all"],
      "bbox_can_dispatch": true
    },
    "_headers": { "x-slack-envelope-id": "replay-004" },
    "type": "interactive",
    "event_type": "block_actions",
    "team_id": "T01",
    "channel": "C01",
    "channel_type": "channel",
    "user": "Ualice",
    "ts": null,
    "thread_ts": null,
    "text": null,
    "subtype": null,
    "bot_id": null,
    "reaction": null,
    "item_ts": null,
    "command": null,
    "command_text": null,
    "response_url": null,
    "trigger_id": "trig-7",
    "action_id": "apply_proposal",
    "action_value": "P-3",
    "view_id": null,
    "view_state_values": null,
    "files": [],
    "raw": {
      "type": "block_actions",
      "user": { "id": "Ualice" },
      "channel": { "id": "C01" },
      "team": { "id": "T01" },
      "actions": [{ "action_id": "apply_proposal", "value": "P-3" }]
    }
  }' | jq
```

Expect: `verdict_classification: "signal_arc"`, `signal: "proposal-applied"`.

### Self-loop / bot message (should be ignored)

```sh
curl -s -X POST http://127.0.0.1:7264/webhook/slack/replay \
  -H 'Content-Type: application/json' \
  -H 'X-Slack-Envelope-Id: replay-005' \
  -d '{
    "_meta": {
      "source": "bro-slack",
      "workspace_id": "T01",
      "self_bot_id": "Bbot",
      "self_user_id": "Ubot",
      "received_at": "2026-05-05T12:34:56.789Z",
      "envelope_id": "replay-005",
      "retry_attempt": 0,
      "bbox_user": "anonymous",
      "bbox_scopes": ["read"],
      "bbox_can_dispatch": false
    },
    "_headers": { "x-slack-envelope-id": "replay-005" },
    "type": "events_api",
    "event_type": "message",
    "team_id": "T01",
    "channel": "C01",
    "channel_type": "channel",
    "user": null,
    "ts": "1730816096.000300",
    "thread_ts": null,
    "text": "bot reply here",
    "subtype": "bot_message",
    "bot_id": "Bbot",
    "reaction": null,
    "item_ts": null,
    "command": null,
    "command_text": null,
    "response_url": null,
    "trigger_id": null,
    "action_id": null,
    "action_value": null,
    "view_id": null,
    "view_state_values": null,
    "files": [],
    "raw": {
      "event": {
        "type": "message",
        "subtype": "bot_message",
        "bot_id": "Bbot",
        "text": "bot reply here",
        "ts": "1730816096.000300",
        "channel": "C01"
      }
    }
  }' | jq
```

Expect: `verdict_classification: "ignore"`.

## Observability

The daemon's generic webhook tools work for Slack the same as for any
other webhook source:

| Goal | Tool |
|---|---|
| Recent deliveries + routing verdicts | `bro_webhook_deliveries(name="slack")` |
| Replay routing without dispatching | `bro_webhook_replay(name="slack", body=..., headers=...)` |
| Active arcs from Slack | `bro_arc_status` |
| Signal match/miss history | `bro_signals(signal=...)` |

The sidecar logs to stderr in `tracing` JSON format. Cross-process
correlation key is `envelope_id`.

## Deployment

### systemd unit

A service template is at `deploy/bro-slack.service`. Install:

```sh
install -m 755 target/release/bro-slack ~/.local/bin/bro-slack
cp deploy/bro-slack.service ~/.config/systemd/user/
systemctl --user daemon-reload
```

Create a drop-in for secrets (never commit these):

```sh
mkdir -p ~/.config/systemd/user/bro-slack.service.d
cat > ~/.config/systemd/user/bro-slack.service.d/secrets.conf <<'EOF'
[Service]
Environment=SLACK_APP_TOKEN=xapp-1-your-app-token-here
Environment=SLACK_SELF_USER_ID=U0BOTUSR0
Environment=SLACK_SELF_BOT_ID=B0BOTBOT0
Environment=BRO_SLACK_SHARED_SECRET=...same value in blackboxd env...
EOF
systemctl --user daemon-reload
systemctl --user enable --now bro-slack.service
```

The service requires `blackbox.service` (Wants+After). Restart policy is
`on-failure` with 3s delay; `bro-slack` handles its own reconnect backoff
internally, so the systemd restart only fires on process crash.

### app manifest

A template Slack app manifest is at `examples/slack/manifest.yaml`.
It covers v1 scopes (§9.2) and Socket Mode enablement. See comments
in the file for install steps. You must review the scopes and event
subscriptions before installing.

### Health endpoint

Run with `--health-port 7299` to expose a loopback-only `/health` endpoint:

```sh
curl http://127.0.0.1:7299/health | jq
```

Returns per §13.1:
```json
{
  "connected": true,
  "uptime_secs": 3712,
  "last_event_at": "2026-05-05T12:34:50Z",
  "events_forwarded": 1284,
  "events_dropped_self_loop": 47,
  "events_dropped_malformed": 3,
  "events_failed_post": 2,
  "events_failed_post_exhausted": 0,
  "reconnects": 1,
  "last_disconnect_reason": "",
  "self_user_id": "U0BOTUSR0",
  "self_bot_id": "B0BOTBOT0",
  "workspace_id": "T01234567"
}
```

## Known limitations (v1)

| Limitation | Impact | Mitigation |
|---|---|---|
| **No persistent buffer** | Daemon down past 3-retry budget → events dropped. Sidecar ack-and-drops with warning log. | Restart daemon promptly. Systemd `After=blackbox.service` ensures daemon starts first. |
| **No `start_arc` idempotency** | Two `app_mention` events on the same thread before the first arc registers a Wait → duplicate arcs race. `bbox_pin` is not atomic. | v1.5: daemon-side `start_arc` idempotency keyed on `(workflow, correlate)`. |
| **Reaction on mid-thread messages** | `reaction_added.item.ts != thread_ts` when a reaction lands on a reply instead of the parent → correlation misses. | Constrain UX to react on the bot's parent message. v1.5: sidecar enriches `parent_thread_ts` via `conversations.replies`. |
| **No outbound 429 handling** | Workflow `http_json` nodes hitting Slack rate limits fail the node; `on_failure: warn` drops the post, `on_failure: halt` terminates the arc. No `retry-after` awareness. | v1.5: expose response headers in `HttpFetchResult` or add Slack-aware retry hook. |
| **In-memory `WaitStore`** | Daemon restart loses every suspended arc. Slack events arriving while the arc is gone correlate to nothing. | v1.5: disk-backed `WaitStore` (daemon-wide, not Slack-specific). |
| **No Phase II ingestion / entity refs** | `file_shared`, channel history, permalink-anchored provenance, Slack entity types in agentic-corpus — all out of scope. | Phase II. |
| **No modal flows or App Home** | Rejection rationale capture via `views.open`/`view_submission`, personal inbox dashboard via `views.publish` — v1.5. | v1 accepts the gap. |
| **Reaction destructive verbs** | `:x:` / `:stop_sign:` for reject/cancel need modal confirmation → v1.5 surface. | v1 ships only non-destructive `:white_check_mark:` approval signal. |
| **Council channels on Slack** | Council streaming + per-persona display needs polling-loop pattern + `chat:write.customize` scope analysis → v1.5. | v1 runs councils via the existing `/council/*` HTTP routes (bro-irc/terminal) only. |

## v1 boundary

See `design/bro-slack.md` §15 for what bro-slack is **not**:
- Not a Slack-specific daemon module (daemon links no Slack crate)
- Not a workflow engine (no retry/state/correlation — just POSTs envelopes)
- Not a router (routing lives in packets)
- Not a token broker (each process reads its own env)
- Not exposed as MCP tools (existing `bro_webhook_*` works for Slack)
- Not an ingestion arc (file/channel indexing is Phase II)

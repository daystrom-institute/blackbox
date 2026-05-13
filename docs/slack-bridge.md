# Slack Bridge

`bro-slack` is a sidecar binary that translates Slack workspace events
into the blackbox daemon's generic webhook + routing pipeline. The daemon
itself carries no Slack-specific code - the sidecar owns the socket,
auth, reconnection, and envelope shape, and the daemon receives
normalized webhook events through the same path as any other ingress
source.

## Architecture

```
Slack Events API ──► bro-slack sidecar ──► POST /webhook/slack ──► blackboxd
                        │                        │                    │
                        │ owns:                  │ carries:           │ route: start_arc
                        │ - socket mode          │ - normalized       │ signal_arc
                        │ - auth (bot token)     │   entity fields    │ ignore
                        │ - reconnection         │ - signature        │
                        │ - message threading    │   verification     │
                        │                        │                    │
                        ◄── HTTP JSON hook ◄──── (workflow node egress)
```

The same pattern applies to any future chat-platform adapter: Discord,
Matrix, Mattermost, Zulip - swap the sidecar, keep the daemon unchanged.

## Setup

### 1. Slack app prerequisites

Create a Slack app from [api.slack.com/apps](https://api.slack.com/apps) with:

- **Socket Mode** enabled
- **Bot Token Scopes**: `chat:write`, `channels:history`, `channels:read`,
  `reactions:read`, `app_mentions:read`
- **Event Subscriptions**: `app_mention`, `reaction_added`, `message.channels`
- Generate an **app-level token** with `connections:write`

### 2. Environment

```bash
# ~/.env.bro-slack or inline
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_APP_TOKEN=xapp-...
export BBOX_DAEMON_URL=http://127.0.0.1:7264
```

### 3. Run as a systemd service

```bash
install -m 755 target/release/bro-slack ~/.local/bin/bro-slack
cp deploy/bro-slack.service ~/.config/systemd/user/
# Edit the service to set SLACK_BOT_TOKEN and SLACK_APP_TOKEN
systemctl --user enable --now bro-slack.service
```

## Channel binding

Bind a Slack channel to a blackbox project so the per-channel triage
workflow knows where to scope proposals and dispatch work:

```json
bro_slack_bind(
  action = "bind",
  team_id = "T0123ABCD",
  channel_id = "C0123XYZ",
  channel_name = "transcript-search",
  project = "/home/invidious/repos/transcript-search"
)
```

Other actions:

```json
// List all bindings
bro_slack_bind(action = "list")

// Look up a specific channel
bro_slack_bind(action = "lookup", team_id = "T0123ABCD", channel_id = "C0123XYZ")

// Unbind
bro_slack_bind(action = "unbind", team_id = "T0123ABCD", channel_id = "C0123XYZ")
```

## Triage workflow

The per-channel triage workflow is the core Slack integration. On a
schedule (or trigger), it:

1. **EnsureInstance** - get-or-create a Badgey consultant for the channel's
   bound project
2. **Synthesize** - Badgey inspects the inbox (stale threads, unresolved
   notes, open work) and writes proposals
3. **ForeachPostProposal** - for each new proposal:
   1. `chat.postMessage` - post the proposal to the Slack channel
   2. `bro_slack_link_record` - record the Slack message → proposal
      mapping so reactions/replies can resolve back
4. **Apply hooks** - `:white_check_mark:` reactions trigger proposal apply;
   thread replies trigger refinement rounds

## Proposal lifecycle

```
Badgey proposes ──► Slack message posted ──► link recorded
                                              │
                      ┌───────────────────────┤
                      ▼                       ▼
              :white_check_mark:        Thread reply
                      │                       │
                      ▼                       ▼
              badgey_apply_proposal     bro_resume (refinement)
                      │                       │
                      ▼                       ▼
              Post outcome + badge      Updated proposal reposted
```

Look up a proposal from a reaction or reply:

```json
bro_slack_link_lookup(
  team_id = "T0123ABCD",
  channel_id = "C0123XYZ",
  msg_ts = "1778179224.543499"
)
```

Record a new link (called by the triage workflow after posting):

```json
bro_slack_link_record(
  team_id = "T0123ABCD",
  channel_id = "C0123XYZ",
  msg_ts = "1778179224.543499",
  proposal_id = "P-3",
  instance_id = "bg-deadbeef-cafef00d",
  project_dir = "/home/invidious/repos/transcript-search"
)
```

## Routing

Inbound Slack events flow through the daemon's standard webhook routing
pipeline:

1. Sidecar translates Slack event → normalized JSON entity
2. `POST /webhook/slack` - signature verification
3. Extractor projects entity fields
4. Routing packet classifies: `start_arc` (launch a workflow for this
   event), `signal_arc` (resume a waiting workflow node), `cancel_arc`,
   `ignore`, or `dead_letter`

This is the same pipeline that handles code-host webhooks (Forgejo,
GitHub). The only Slack-specific code lives in the sidecar's translation
layer.

## Design notes

For the full design rationale - why a sidecar rather than an embedded
Slack crate, the process-supervision pattern shared with `bro-irc`, and
known gaps - see [`design/bro-slack.md`](https://github.com/invidious9000/transcript-search/blob/main/design/bro-slack.md) and
[`design/bro-slack-next.md`](https://github.com/invidious9000/transcript-search/blob/main/design/bro-slack-next.md).

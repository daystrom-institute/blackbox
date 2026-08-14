---
title: bro-slack — Slack sidecar bridge for blackbox
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - integrations
  - slack
tags:
  - integrations
---

# bro-slack — Slack sidecar bridge for blackbox

## 1. Thesis

Tying blackbox to Slack should not move the daemon's core. The daemon
already has the right substrate — generic webhook ingress with extractors
and signature schemes, routing packets that classify events into
start_arc/signal_arc/ignore, `http_json` hook for arbitrary outbound
calls, `Wait` nodes with correlation tuples and timeouts, gates as
packets, threads and notes and the inbox layer. Slack's ingress envelope
is JSON; Slack's egress is authenticated HTTP POST. Both are largely
expressible against existing primitives, with a few specific gaps named
honestly in §14.

What Slack needs is a **transport adapter**: one process that owns the
Slack-specific socket, authentication, reconnection, and envelope shape,
and translates each inbound event into a normalized webhook envelope the
daemon's existing `/webhook/<name>` endpoint accepts. Outbound Slack
posts ride the existing `http_json` hook from inside workflow nodes. The
daemon never links a Slack crate, never carries Slack-specific signature
schemes, never parses a Slack-specific payload.

This is **`bro-slack`**: a sidecar binary, process-supervised the same
way `bro-irc` is supervised, but speaking a different shape on the
daemon side. Where `bro-irc` translates IRC commands into typed
`/irc/*` and `/council/*` HTTP RPCs, `bro-slack` translates Slack
events into the *generic webhook envelope* — the daemon-side contract
already documented for code-host integrations like Forgejo. The two
sidecars share the process-supervision pattern; they do not share the
HTTP contract.

The opposite framing — Slack-aware daemon with embedded Events API
listener, slack signature scheme, slack entity types in the entity-ref
parser — couples the daemon to one chat platform. The day a team wants
Discord, Matrix, Mattermost, or Zulip, every coupling has to be
reproduced. Sidecar-as-transport-adapter means the daemon never moves;
the adapter is replaceable.

## 2. Architecture

Three layers, each with one job.

| Layer | Responsibility | Owns |
|---|---|---|
| **bro-slack sidecar** | Slack transport — Socket Mode WebSocket, signature verification (where applicable), envelope normalization, ack semantics, reconnect/backoff, ACL enrichment | Slack app-level token; signing secret; WebSocket lifecycle; one normalized POST per Slack event |
| **blackboxd** | Routing, workflow execution, hook dispatch — unchanged at v1 | Generic `/webhook/<name>` endpoint; routing packets; workflow JSON; `http_json` hook; Slack bot token (for outbound) |
| **Workflow JSON** | Slack-specific product logic — what a mention does, what a reaction signals, how a Block Kit message looks | All Slack semantics; lives in user repos under `<project>/.bbox/` |

The seam between layers is intentional. The sidecar speaks one protocol
in (Slack Socket Mode) and one protocol out (daemon webhook envelope).
The daemon speaks generic webhook in and authenticated HTTP out. The
product layer (workflow JSON) decides what a Slack mention or reaction
means.

### 2.1 Process topology

```
┌────────────────┐                   ┌─────────────────────────┐
│   slack.com    │ wss://socket-mode │  bro-slack (sidecar)    │
│                │ ◄────────────────►│  - holds APP token      │
│   Web API      │                   │  - holds signing secret │
└────────────────┘                   │  - normalizes envelope  │
       ▲                             │  - enriches ACL         │
       │ https://slack.com/api/*     └─────────────┬───────────┘
       │ from workflow http_json                   │ POST 127.0.0.1:7264
       │ using BOT token                           │     /webhook/slack
       │                                           ▼
       │                             ┌─────────────────────────┐
       └─────────────────────────────┤   blackboxd (daemon)    │
                                     │  - routing packets      │
                                     │  - workflow engine      │
                                     │  - http_json hook       │
                                     │  - holds BOT token      │
                                     └─────────────────────────┘
```

Three trust boundaries: Slack ↔ sidecar (over the public internet,
mutually authenticated by the Socket Mode app token), sidecar ↔ daemon
(loopback by default; HMAC-signed shared-secret in hardened mode),
daemon ↔ Slack Web API (outbound TLS, authenticated by the bot token).
The daemon never opens an inbound socket on a non-loopback interface
for Slack's sake.

### 2.2 Token isolation, honestly

Three Slack-issued credentials, each with a different blast radius.
The sidecar holds the two with the highest privilege; the daemon holds
the outbound-bot one.

| Credential | Lives in | What it grants if compromised |
|---|---|---|
| App-level token (`xapp-1-*`) | Sidecar env only | Open Socket Mode connections; receive every event the app subscribes to. **Highest privilege Slack credential we hold.** Never leaves the sidecar. |
| Signing secret | Sidecar env only | Verify Events API request signatures (unused on the Socket Mode happy path; held for Events API fallback or testing). Never leaves the sidecar. |
| Bot user token (`xoxb-*`) | Daemon env | Outbound Web API actions: post messages, open modals, fetch user info. Scoped to the bot's granted scopes (§8.1). |
| Sidecar shared secret | Sidecar env + daemon env | HMAC the sidecar→daemon hop in hardened mode. |

This is not full isolation — the daemon still holds `SLACK_BOT_TOKEN`
because workflow `http_json` hooks need it for outbound. The honest
claim is narrower than "Slack tokens live in the sidecar":

- The Socket Mode + signing-secret credentials, which are workspace-
  wide and high-privilege, never enter the daemon.
- The bot token, which is scoped per OAuth grant and is the credential
  most Slack apps treat as primary, lives in the daemon. A daemon
  compromise leaks bot-scoped Web API access; it does not leak the
  ability to receive events or impersonate the app at the Socket
  Mode layer.
- Token rotation is independent: rotate the bot token by restarting
  the daemon; rotate the app token by restarting the sidecar.

### 2.3 Why sidecar, not in-daemon

- **Loopback posture preserved.** The CLAUDE.md-codified default for
  `blackboxd` is loopback-only on `127.0.0.1:7264`. Embedding an Events
  API listener requires a public ingress with TLS termination;
  embedding Socket Mode pulls a WebSocket dependency into the daemon.
  Sidecar keeps the daemon untouched.
- **Crash isolation.** Slack reconnect storms, rate-limit pauses, and
  WebSocket library bugs happen in a separate process. The daemon keeps
  serving MCP, tantivy, and orchestration through any sidecar incident.
- **Independent deploy.** Sidecar restarts to rotate app-level tokens
  or upgrade the Slack crate without touching `blackboxd`.
- **Templateability is a side benefit, not the load-bearing argument.**
  Discord/Matrix/Mattermost/Zulip *could* follow the same skeleton
  someday, but the doc doesn't claim drop-in adapter parity until a
  second adapter exists. See §17.

## 3. Sources & prior art

- `src/irc_bridge.rs` — process-supervision and chat-event-as-HTTP-RPC
  pattern. **Important caveat:** `bro-irc` does NOT use the generic
  webhook envelope. It calls typed daemon routes (`/irc/exec`,
  `/irc/status`, `/council`, `/council/{id}/post`) and tails `/tail` +
  council SSE. `bro-slack` proposes a *different* daemon-side contract
  — the same `/webhook/<name>` endpoint that Forgejo uses — because
  Slack events fit a routing-packet classification model better than
  IRC commands do, and because we want to drive workflows from Slack
  events (which routing packets enable) rather than fire one-shot
  RPCs (which the IRC bridge was designed for). The sidecars are
  cousins, not siblings.
- `examples/keystone/keystone-example.md`, `examples/keystone/webhooks/forgejo.json`,
  `examples/keystone/packets/routing-forgejo.json` — the canonical
  webhook → extractor → routing-packet → workflow flow. The daemon
  side of bro-slack matches this contract.
- [Workflow Engine](../../../docs/workflows.md) — engine semantics: Wait correlation tuples, gate
  packets, hook ops, the in-memory `WaitStore` (§14), poller inlets.
- `src/packets.rs` — actual predicate vocabulary used in routing packets
  (§6.1). The doc cites these literally.
- `src/orchestration/http_fetch.rs` — current `http_json` capabilities
  and limits (§7, §14).
- `design/corpus/badgey.md` §6.3 (inbox triage), §6.4 (close-loops), §6.5
  (proposals) — the conversational surfaces that benefit most from
  Slack interactivity.
- `design/corpus/agentic-corpus/agentic-corpus.md` §6 (entity refs), §7.3 (chunkers) — the
  substrate the Phase II Slack-as-entity tie-in slots into.
- `slack-blocks` (Rust crate) — Block Kit JSON builders. Optional
  ergonomic dependency for sidecar-side Block composition; not
  required.
- `slack-morphism` — comprehensive Rust Slack crate. Surveyed but not
  adopted: its surface (full Web API + Events + Socket Mode +
  Block Kit + OAuth + Hyper/Axum adapters) is much larger than what
  the sidecar needs. Bias is hand-roll on `tokio-tungstenite` plus
  `reqwest`; revisit if we hit a real ergonomic wall.

## 4. Phasing

The doc names a comprehensive capability surface (§11) deliberately —
this is a design document, not a sprint plan. Phase tags below map each
capability to a tier. Implementation phases will live in a companion
`bro-slack-impl.md` once this doc converges.

| Tier | What lands |
|---|---|
| **v1** | Outbound notifications via `http_json`; sidecar binary with Socket Mode; webhook envelope contract; routing-packet domain; slash-command and app-mention handling; reaction-as-signal for non-destructive verbs; Block Kit approval messages |
| **v1.5** | Per-user identity claim flow (§9); App Home rendering of `bbox_inbox`; modal-based cosession kickoff; modal rejection-rationale capture; durable retry/backoff for outbound throttling (§14); council channels with polling-loop streaming; daemon-side `start_arc` idempotency; persistent `WaitStore` |
| **Phase II** | Slack as a first-class entity in agentic-corpus (§12.9 + §12.10); file ingestion via the agentic-corpus chunker registry (§12.8); permalink-anchored provenance. Ownership moved: these ingestion items are now owned by `design/connectors/slack-ingestion-connector.md` |
| **Future possibility** | Multi-workspace Enterprise Grid; one-app-per-persona for council voice fidelity; daemon-side Slack signature awareness (only if the sidecar pattern is abandoned) |

## 5. Sidecar binary — `bro-slack`

### 5.1 Lifecycle

```
1. Start: read SLACK_APP_TOKEN, SLACK_SIGNING_SECRET, --self-user-id,
   --self-bot-id from env/CLI. Bot token NEVER touches the sidecar.
2. Call apps.connections.open with app token → returns wss:// URL.
3. Open WebSocket; receive events.
3b. Open the durable spool directory (§5.6) and replay anything a
   previous process left undelivered. Failing to open the spool is
   fatal: a sidecar that cannot write the spool cannot honestly ack.
4. ACCEPTANCE, for each event envelope. Runs to completion on the
   socket task; never cancelled, never deadline-cut (see below):
     a. Filter loop-back (event.user == self_user_id OR
        event.bot_id == self_bot_id → drop). Nothing to deliver, so
        this acks without spooling.
     b. Normalize to bbox envelope (§6).
     c. Enrich with ACL: look up event.user in the identities file
        (§9), attach `_meta.bbox_user` and `_meta.bbox_scopes` and
        `_meta.bbox_can_dispatch`. Unmapped users get
        `bbox_user: "anonymous"`, `bbox_scopes: ["read"]`,
        `bbox_can_dispatch: false` (§6.3).
     d. Write the normalized envelope to the durable spool, fsync the
        file, rename, fsync the directory (§5.6). On ANY failure:
        WITHHOLD the ack, release the in-flight claim, log at error
        level, and let Slack redeliver.
     e. Ack to Slack. The envelope is now durable locally, so Slack
        can forget it.
     f. Enqueue it for delivery on a bounded worker queue. A full
        queue is not an error: the entry is durable, so the sweep
        picks it up.
5. DELIVERY, on the worker (or a drain lane), after the ack:
     a. Take the envelope's delivery lease. Held elsewhere means
        another lane owns it; do nothing.
     a2. Re-read the CURRENT spool entry by id. Gone means another lane
        already delivered it; do nothing. Queue entries and drain
        snapshots carry ids only, never bodies, so a stale request can
        never be POSTed.
     b. POST to http://127.0.0.1:7264/webhook/slack with envelope.
        Header: X-Slack-Envelope-Id: <id>.
        On 2xx: delete the spool entry.
        On non-2xx: retry up to 2 more times with 500ms then 1s
        sleeps between attempts (3 POST attempts total, ~1.5s of
        sleep). After the 3rd attempt fails, the envelope STAYS in
        the spool, stamped with the attempt count and last error,
        for the retry sweep (§5.6).
     c. Three consecutive failed rounds gate the endpoint for an
        escalating 5s to 60s window; gated lanes skip the POST and
        leave entries untouched.
6. Every --spool-sweep-secs (default 300): re-attempt spooled
   envelopes that are past the quiet period, in batches of 200, and
   discard entries past the age bound with a structured error log.
7. On disconnect: exponential backoff reconnect (1s → 60s cap).
8. On SIGTERM: stop accepting new events, let the delivery worker
   finish what it holds (≤5s grace), close socket, exit 0. Anything
   undelivered is already in the spool and is replayed on the next
   start.
```

**The phase split is the safety property, not a structural
preference.** Acceptance ends by telling Slack to forget the
envelope, and Slack has no server-side replay, so a cancellation
between the durable write and the ack loses the event, and an ack
emitted because a shutdown deadline expired loses an event whose
spool write may never have landed. Acceptance is therefore bounded by
one fsync plus one socket write and is allowed to finish even during
shutdown, which it can do quickly precisely because it does no
network I/O. Delivery, which does, is the only phase a deadline may
cut, and by then everything it touches is durable.

Two timing constraints used to overlap. Slack's Socket Mode ack
deadline is ~3 seconds; exceeding it triggers Slack-side redelivery,
while a 3-attempt POST retry budget can reach ~4.5s against a slow
daemon. Moving the ack to step 4e (after the durable write, before
any POST) decouples the two: the ack now depends only on a local
fsync, so daemon latency can no longer push the sidecar past Slack's
deadline, and moving delivery off the socket task keeps the reader
responsive while a round is in flight.

Redelivery dedup still matters for the residual window between
receiving a frame and completing the spool write. Two layers defend
it: the daemon's `X-Slack-Envelope-Id` header dedup (§6.5) catches
POSTs that reach the daemon, and an in-sidecar in-flight-envelope-id
set drops a Slack-redelivered envelope before a second spool write is
issued. The sidecar holds the in-flight set for 30s. The one case
where the claim is deliberately RELEASED is a failed spool write:
there the sidecar is counting on redelivery, so holding the claim
would dedupe away its own recovery path.

The ack-only-after-durability rule in steps 4d/4e is load-bearing.
Acking before anything is durable means events are silently lost on
daemon downtime; acking after a local durable write hands ownership
to the sidecar, which then owns delivery until 2xx or the age bound.

### 5.2 CLI shape

```
bro-slack [OPTIONS]
  --app-token-env       env var holding xapp-* (Socket Mode)        [SLACK_APP_TOKEN]
  --signing-secret-env                                              [SLACK_SIGNING_SECRET]
  --self-user-id        bot's own U-prefix user id                  required
  --self-bot-id         bot's own B-prefix bot id                   required
  --daemon-url          base URL of blackboxd                       [http://127.0.0.1:7264]
  --webhook-name        endpoint name on daemon side                [slack]
  --shared-secret-env   optional HMAC key for sidecar→daemon hop    [BRO_SLACK_SHARED_SECRET]
  --identities-file     ACL mapping file                            [<bro_home>/slack-identities.json]
  --health-port         optional health endpoint port (off by default)
  --spool-dir           durable envelope spool directory            [<bro_home>/slack-spool]
  --spool-sweep-secs    gap between spool retry sweeps, 0 disables  [300]
  --spool-max-age-secs  age at which a spooled envelope is dropped  [86400]
  --spool-max-entries   spool entry cap, 0 means unbounded          [5000]
  --log-level                                                       [info]
```

Per-channel and per-event subscriptions are configured at app-config
time on api.slack.com (event subscriptions); routing packets gate
which events get acted on. The sidecar carries no subscription config.

### 5.3 Dependency choice

Hand-roll on `tokio-tungstenite` (new dependency — `tungstenite` is NOT
already in tree; `bro-irc`'s `irc` crate uses native sockets) plus
`reqwest` (already in tree). Optional `slack-blocks` only if outbound
Block Kit JSON gets composed sidecar-side, which v1 does not — outbound
Block Kit lives in workflow JSON.

`slack-morphism` is the heavyweight alternative if we want builder
ergonomics for every Slack endpoint, at the cost of pulling its full
dep tree. Bias: hand-roll until concrete pain motivates the switch.

### 5.4 Self-loop filter

Slack delivers the bot's own posts as `message` events back over the
socket. **Slack distinguishes `bot_id` (B-prefix, the Slack app's bot
integration ID) from `user_id` (U-prefix, the bot user's identity).**
Both are populated for the bot; both must be filtered.

Two-layer filter:
- Sidecar drops events where `event.user == self_user_id` OR
  `event.bot_id == self_bot_id` before forwarding.
- Routing packet (defense in depth) carries an `ignore_bot_messages`
  rule with `IsNonNull` against `subtype` matching `bot_message` or
  `bot_id`.

### 5.5 Reconnect & rate limit

Socket Mode reconnects are part of the protocol — Slack sends
`disconnect` with a `reason`; sidecar reopens. Outbound rate limits
don't apply on the sidecar (it doesn't post outbound; workflow nodes
do). 429 handling for outbound is a known v1 gap — see §14.

### 5.6 Durable envelope spool

Socket Mode has no server-side replay. Once the sidecar acks an
envelope_id, Slack forgets it, so an ack is a promise the sidecar has
to keep. The original v1 sidecar acked after its POST retry budget ran
out and dropped the envelope, which meant any daemon outage longer
than ~4.5s silently lost every event that landed inside it. The spool
retires that drop.

**Location.** A flat directory, one JSON file per envelope, defaulting
to `<bro_home>/slack-spool`: the same resolved state root that holds
`slack-identities.json`, so sidecar state does not fan out across the
filesystem. `--spool-dir` overrides it (with `~` expansion), mirroring
the `--identities-file` convention.

**Entry.** `{version, envelope_id, spooled_at, attempts,
last_attempt_at, last_error, event}`. The stored `event` is the
NORMALIZED, ACL-enriched body, not the raw Socket Mode frame:
re-normalizing at replay time would re-read the identity map and could
attribute a message to a different bbox user than the one in effect
when it arrived.

**Crash safety.** Write to `<name>.json.tmp`, fsync the file, rename
into place, then fsync the directory. A rename that lands ahead of its
data is precisely the crash window the spool exists to close. The
filename derives from the envelope_id with everything outside
`[A-Za-z0-9_-]` replaced and the length bounded (the id arrives over
the network and is never trusted as a path component), plus a short
digest of the original id so two ids cannot collide onto one file.
Unparseable or unknown-version files are renamed to `.json.corrupt`
rather than deleted or re-read forever.

Directory sync is NOT best effort on this path. A rename whose
directory entry is lost by a crash, followed by an ack that already
told Slack to forget the envelope, is the exact loss the spool exists
to prevent, so a failed directory fsync fails the write and withholds
the ack. The stamp and delete paths downgrade it: losing an attempt
counter costs a retry, and losing an unlink costs a duplicate the
daemon absorbs.

A failed sync still COUNTS the entry, though. The rename has already
landed by then, so the file is visible to every reader; leaving it out
of the depth counter understates the spool, which makes the drain skip
its inventory on a depth of zero and lets later admissions slip past
the entry cap. The write reports failure (so the ack is withheld) while
the accounting reflects what is actually on disk. A Slack redelivery
then finds the entry already present and republishes it without
double counting.

**Accounting is serialized.** Every entry-path and depth mutation
(admit, remove, stamp, inventory) runs under one lock. The cap is
read-modify-write, so unserialized admissions each see the
pre-eviction depth and overshoot; and a persist that probes an id as
present, races a concurrent remove, republishes, and skips the
increment it now owes leaves the same visible-but-uncounted file as
the sync case. One lock over all four operations removes the family
rather than patching the instances.

**Acceptance and delivery are separate phases.** Acceptance is
normalize, spool, ack, hand off. It is bounded by one fsync plus one
socket write and is never cancelled, never deadline-cut, and never
raced against shutdown: a cancellation between the durable write and
the ack, or an ack emitted because a deadline expired, loses the
envelope outright. Delivery is everything after the ack and is the
only phase a deadline may cut, because everything it touches is
already durable.

Delivery therefore runs on a bounded worker queue rather than on the
socket task. A delivery round can burn ~4.5s against a sick daemon,
and the socket reader cannot afford to wait through that: Slack keeps
pushing frames, and a blocked reader turns one slow daemon into a
backlog of unacked envelopes. A full queue is backpressure, not loss.
The envelope is already durable, so overflow simply falls to the
sweep.

**Delivery lanes.** Three, all going through the same lease, breaker,
POST path, and settlement, and therefore the same daemon-side
`X-Slack-Envelope-Id` dedup:

- *Worker*, immediately after the ack, so the happy path is unchanged
  in latency.
- *Boot replay*, on start, with no quiet period. It runs alongside the
  Socket Mode connection rather than ahead of it, so a large backlog
  cannot delay accepting live traffic. Slack does not guarantee event
  ordering in the first place. It keeps issuing batches while batches
  keep making progress (capped at 64), because one batch is bounded at
  200 entries and a one-shot replay would strand everything past that
  until some later pass. A batch that achieves nothing stops the loop
  rather than spinning on a daemon that is down.
- *Retry sweep*, every `--spool-sweep-secs` (default 300s). It skips
  entries touched within a 60s quiet period so it does not churn on
  work the worker is about to do. That quiet period is an efficiency
  filter, not a safety mechanism: the lease and the entry re-read are
  what make lane overlap safe.
- The drain lane also wakes ON DEMAND, independently of the timer,
  when the delivery queue overflows. Deferring an envelope to "the
  sweep" is only sound if a sweep is coming, and `--spool-sweep-secs 0`
  means none is. A wakeup pass uses no quiet period, because the
  wakeup means the worker explicitly did not take that envelope.
  Setting the interval to 0 therefore disables the PERIODIC lane only,
  not the drain lane: the documented consequence is that an envelope
  whose delivery round fails then waits for the next overflow wakeup or
  the next process start.

The boot replay and the sweep share one task, so drains never overlap
each other, and every lane takes a per-envelope delivery lease before
touching an entry. The lease is what makes CONCURRENT lanes safe:
without it two lanes can POST one envelope at once and, worse, race
each other's settlement, where one deletes the entry while the other
stamps a failure onto it and resurrects a delivered envelope. The
lease is in-process; two sidecars sharing one spool directory is not a
supported deployment.

The lease alone does not cover SEQUENTIAL delivery from stale state,
which is the subtler half. A drain snapshots the spool, the worker
delivers and removes one of those envelopes, and only then does the
drain reach it: by that point the lease is free, so a lane holding the
old body would happily POST it again. Two rules close it. Queue
requests and drain snapshots carry only envelope ids, never
independently deliverable bodies; and delivery re-reads the current
entry by id after taking the lease, treating a missing entry as
already settled. An envelope's body is therefore only ever read from
the entry that still exists at the moment of the POST.

Drains are sequential and batched at 200 entries per pass, with the
deferred remainder logged rather than silently dropped. A drain runs
precisely when the daemon has been unhealthy, so firing an unbounded
backlog at it the moment it returns is the wrong first move, and an
unbounded pass would also delay age-bound discards behind thousands of
doomed POSTs.

**Endpoint breaker.** After three consecutive failed delivery rounds
the daemon endpoint is gated for an escalating 5s to 60s window. While
gated, lanes skip POSTing entirely and leave entries untouched, and a
drain that hits the gate abandons the pass. Without this, a spool of
thousands against a daemon that is down spends ~4.5s per entry
achieving nothing. Any success reshuts the breaker.

**Growth bounds.** Two on entries, both loud, neither silent, plus
bounds on the incidental artifacts:

- *Age*: `--spool-max-age-secs`, default 86400 (24h). Long enough to
  cover an overnight daemon outage, short enough that the sidecar is
  not replaying two-day-old slash commands into a workspace whose
  threads have moved on and whose arcs have timed out. Discard emits a
  structured error naming the envelope_id, age, attempt count, and
  last error, and increments `events_spool_discarded_aged`. Age beats
  quiet time in the sweep's decision order, otherwise a permanently
  failing envelope retried every pass would never reach the bound.
- *Count*: `--spool-max-entries`, default 5000 (roughly 20MB at a few
  KB per envelope). At the cap the OLDEST entries are evicted to admit
  the newest. Shedding fresh traffic to preserve a day-old backlog is
  the wrong trade. Two ordering rules matter here: whether an id
  already has an entry is decided BEFORE eviction runs, so re-spooling
  an existing envelope at the cap does not evict an unrelated one for
  a slot it will not consume; and admission is serialized, so
  concurrent writes cannot each see the pre-eviction depth and
  overshoot. If the spool cannot be brought under the cap, the write
  is REFUSED and the ack withheld, rather than acking past the bound.
  Each eviction is an error log plus `events_spool_evicted_overflow`.
- *Artifacts*: orphaned `.tmp` files older than an hour are deleted,
  and `.corrupt` quarantines are capped at the newest 50. A crash loop
  would otherwise leave unbounded partials, and a systematic parse bug
  would fill the directory with quarantine copies.

**What the spool does not do.** It is not an ordering guarantee, not a
transaction log, and not a substitute for the daemon's own
`X-Slack-Envelope-Id` dedup: replay can re-POST an envelope the daemon
already accepted (a crash between the daemon's 2xx and the spool
delete, or a delete whose unlink was lost), and the daemon is the
layer that makes that harmless. It also does not cover the
pre-acceptance window: a crash between reading the WebSocket frame and
completing the spool write relies on Slack redelivery, which is
exactly why the ack is withheld on a failed spool write.

## 6. Webhook envelope — sidecar to daemon

The contract between sidecar and daemon. Stable; routing packets
depend on this shape.

### 6.1 POST body shape

Top-level fields are the projected, routing-friendly subset. `raw` is
the full Slack payload. `_meta` is sidecar-emitted and includes
ACL-enrichment.

```json
{
  "_meta": {
    "source": "bro-slack",
    "workspace_id": "T01234567",
    "self_bot_id": "B0BOTBOT0",
    "self_user_id": "U0BOTUSR0",
    "received_at": "2026-05-05T12:34:56.789Z",
    "envelope_id": "1f2e3d4c-...",
    "retry_attempt": 0,
    "bbox_user": "alice",
    "bbox_scopes": ["all"],
    "bbox_can_dispatch": true
  },
  "_headers": {
    "x-slack-envelope-id": "1f2e3d4c-..."
  },
  "type": "events_api",
  "event_type": "app_mention",
  "team_id": "T01234567",
  "channel": "C0CHAN001",
  "channel_type": "channel",
  "user": "U0USER001",
  "ts": "1730816096.000300",
  "thread_ts": "1730816060.000100",
  "text": "<@U0BOTUSR0> can you triage #ops",
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
  "raw": { "...": "untouched Slack payload" }
}
```

### 6.2 Field provenance

To eliminate ambiguity about Socket Mode envelope vs Slack payload
fields:

- `_meta.envelope_id` and `_headers.x-slack-envelope-id` come from the
  Socket Mode envelope (`envelope_id` field). They are NOT a Slack
  payload field.
- Top-level `type` is a normalized discriminator the sidecar emits
  (`events_api` | `interactive` | `slash_commands`) based on which
  Socket Mode envelope shape arrived.
- `event_type` is the Slack payload's `event.type` for `events_api`
  envelopes; null otherwise.
- `command`, `command_text`, `response_url`, `trigger_id` are Slack
  payload fields for `slash_commands`.
- `action_id`, `action_value`, `view_id`, `view_state_values`,
  `trigger_id` are Slack payload fields for `interactive` envelopes.
- `view_state_values` is projected from `view.state.values` on
  `view_submission` payloads, projected as a flat
  `{block_id: {action_id: value}}` map for routing convenience.
- `raw` is the verbatim Slack payload (the `payload` field of the
  Socket Mode envelope). It rides on the wire to the daemon, but the
  daemon's delivery record stores the **extracted entity**, not the
  full body — so routing packets, workflows, AND
  `bro_webhook_deliveries` only see fields the extractor emits. The
  default extractor (§6.5) projects `raw_event` (= `$.raw.event`),
  which covers most events_api needs; reaching deeper into `raw`
  requires adding an extractor output. There is no implicit "all of
  raw" passthrough.

### 6.3 ACL enrichment

`_meta.bbox_user`, `_meta.bbox_scopes`, and `_meta.bbox_can_dispatch`
are populated by the sidecar from the identities file (§9). Routing
packets gate on these fields, not on raw `user` IDs. Why: routing
packets cannot read external files (the predicate vocabulary in
`src/packets.rs` is scalar/regex/quantified patterns over the entity),
so ACL must be projected into the entity before the routing engine
sees it.

Three concrete fields, derived sidecar-side:

- `bbox_user`: identity-file lookup; defaults to `"anonymous"` for
  unmapped users (NOT null — `IsNonNull bbox_user` always succeeds
  so anonymous users can still hit read-only routes).
- `bbox_scopes`: `["all"]` / `["read"]` / etc. from identities file;
  defaults to `["read"]` for anonymous.
- `bbox_can_dispatch`: precomputed boolean — true iff `bbox_scopes`
  contains `"all"`. Sidecar projects this so routing packets do
  scalar `Eq` checks instead of array-element predicates. This avoids
  needing the `Exists{path:"bbox_scopes[*]", pred:...}` quantified
  form for every dispatch gate.

### 6.4 Sidecar→daemon authentication

Two operating modes:

- **Loopback-only** (default): the sidecar runs on the same host as
  the daemon; daemon binds `127.0.0.1`; the webhook spec uses
  `signature.kind: none`. Trust boundary is the loopback interface.
- **Shared-secret HMAC** (multi-host or hardened): the sidecar signs
  each POST with `X-Bro-Sidecar-Signature: <hex>` over the body. The
  webhook spec uses
  `signature: { kind: "hmac_sha256", secret_env: "BRO_SLACK_SHARED_SECRET", header: "X-Bro-Sidecar-Signature" }`.
  `secret_env` and `header` are required by `src/webhooks.rs`; the v1
  doc draft elided them and was malformed.

Slack's own `X-Slack-Signature` and `X-Slack-Request-Timestamp` headers
are verified inside the sidecar against `SLACK_SIGNING_SECRET` for
the HTTP Events API path — unused on the Socket Mode happy path,
where the WebSocket itself is authenticated by the app token. The
daemon never sees Slack-issued signatures.

### 6.5 Daemon-side webhook spec

Hand-installed `webhooks/slack.json`:

```jsonc
{
  "name": "slack",
  "signature": {
    "kind": "hmac_sha256",
    "secret_env": "BRO_SLACK_SHARED_SECRET",
    "header": "X-Bro-Sidecar-Signature"
  },
  "delivery_id_header": "X-Slack-Envelope-Id",
  "extractor": {
    "outputs": {
      "type":         { "kind": "json_path", "path": "$.type" },
      "event_type":   { "kind": "default", "inner": { "kind": "json_path", "path": "$.event_type" }, "fallback": "" },
      "team_id":      { "kind": "json_path", "path": "$.team_id" },
      "channel":      { "kind": "json_path", "path": "$.channel" },
      "channel_type": { "kind": "json_path", "path": "$.channel_type" },
      "user":         { "kind": "json_path", "path": "$.user" },
      "ts":           { "kind": "json_path", "path": "$.ts" },
      "thread_ts":    { "kind": "json_path", "path": "$.thread_ts" },
      "text":         { "kind": "json_path", "path": "$.text" },
      "reaction":     { "kind": "json_path", "path": "$.reaction" },
      "item_ts":      { "kind": "json_path", "path": "$.item_ts" },
      "command":      { "kind": "json_path", "path": "$.command" },
      "command_text": { "kind": "json_path", "path": "$.command_text" },
      "response_url": { "kind": "json_path", "path": "$.response_url" },
      "trigger_id":        { "kind": "json_path", "path": "$.trigger_id" },
      "action_id":         { "kind": "json_path", "path": "$.action_id" },
      "action_value":      { "kind": "json_path", "path": "$.action_value" },
      "view_id":           { "kind": "json_path", "path": "$.view_id" },
      "view_state_values": { "kind": "json_path", "path": "$.view_state_values" },
      "files":             { "kind": "json_path", "path": "$.files" },
      "subtype":           { "kind": "json_path", "path": "$.subtype" },
      "bot_id":            { "kind": "json_path", "path": "$.bot_id" },
      "raw_event":         { "kind": "json_path", "path": "$.raw.event" },
      "bbox_user":         { "kind": "json_path", "path": "$._meta.bbox_user" },
      "bbox_scopes":       { "kind": "json_path", "path": "$._meta.bbox_scopes" },
      "bbox_can_dispatch": { "kind": "json_path", "path": "$._meta.bbox_can_dispatch" }
    }
  },
  "routing_packet": "domain:webhook-routing/slack",
  "default_project_dir": null
}
```

Idempotency dedup is by `X-Slack-Envelope-Id` HTTP header. The current
daemon (`src/main.rs:5808`) reads only headers for delivery id; the
sidecar promotes `envelope_id` to a header to satisfy this. Body-field
fallback would be a daemon change and is out of scope.

## 7. Routing — Slack semantics live in packets

All Slack-specific business logic is one packet domain:
`domain:webhook-routing/slack`. Same shape as keystone's
`webhook-routing/forgejo`.

### 7.1 Packet shape

```jsonc
{
  "domain": "webhook-routing/slack",
  "scope": "global",
  "classification_lattice": ["start_arc", "signal_arc", "ignore", "dead_letter"],
  "rules": [
    /* see 7.2 */
  ]
}
```

### 7.2 Canonical rule shapes

Predicates available in routing antecedents are exactly those
implemented in `src/packets.rs`: `Eq`, `In`, `All`, `Any`, `Not`,
`IsNonNull`, `KeyExists`, `IsNull`, `StringContains`,
`StringMatches`, plus the quantified `Exists{path,pred}` and
`ForAll{path,pred}`. The rules below use a subset; reach for the
others as new rules need them. Earlier doc drafts used `StartsWith`
and the unquantified `Exists{field}` — neither exists; substitute
`StringMatches` (regex) and `IsNonNull` respectively.

```jsonc
[
  // App mention with sufficient scope → start a badgey-ask arc.
  {
    "id": "start_badgey_on_app_mention",
    "classification": "start_arc",
    "antecedent": {
      "op": "All",
      "args": [
        { "op": "Eq", "field": "type",              "value": "events_api" },
        { "op": "Eq", "field": "event_type",        "value": "app_mention" },
        { "op": "Eq", "field": "bbox_can_dispatch", "value": true }
      ]
    },
    "consequent": "{\"route\":\"start_arc\",\"workflow\":\"slack-badgey-ask\",\"initial_vars\":{}}"
  },

  // Anonymous / read-only mention → read-only badgey route.
  {
    "id": "start_readonly_badgey_on_app_mention",
    "classification": "start_arc",
    "antecedent": {
      "op": "All",
      "args": [
        { "op": "Eq", "field": "type",              "value": "events_api" },
        { "op": "Eq", "field": "event_type",        "value": "app_mention" },
        { "op": "Eq", "field": "bbox_can_dispatch", "value": false }
      ]
    },
    "consequent": "{\"route\":\"start_arc\",\"workflow\":\"slack-badgey-readonly\",\"initial_vars\":{}}"
  },

  // Reaction → resume on a typed signal. The correlation key is the
  // bot's first reply ts (which equals thread_ts for threads the bot
  // started). See §11.2 for the item_ts vs thread_ts caveat.
  {
    "id": "signal_proposal_approved_on_check",
    "classification": "signal_arc",
    "antecedent": {
      "op": "All",
      "args": [
        { "op": "Eq", "field": "event_type", "value": "reaction_added" },
        { "op": "Eq", "field": "reaction",   "value": "white_check_mark" }
      ]
    },
    "consequent": "{\"route\":\"signal_arc\",\"signal\":\"proposal-approved\",\"correlate\":{\"thread_ts\":\"${entity.item_ts}\"}}"
  },

  // Slash command from an authorized user.
  {
    "id": "start_triage_on_slash",
    "classification": "start_arc",
    "antecedent": {
      "op": "All",
      "args": [
        { "op": "Eq", "field": "type",              "value": "slash_commands" },
        { "op": "Eq", "field": "command",           "value": "/bbox" },
        { "op": "Eq", "field": "bbox_can_dispatch", "value": true }
      ]
    },
    "consequent": "{\"route\":\"start_arc\",\"workflow\":\"slack-bbox-command\",\"initial_vars\":{}}"
  },

  // Block Kit button click → resume an arc waiting on this proposal.
  // The correlation key is `action_value`, not `action_id`. See §7.3.
  {
    "id": "signal_block_action_apply_proposal",
    "classification": "signal_arc",
    "antecedent": {
      "op": "All",
      "args": [
        { "op": "Eq",        "field": "type",      "value": "interactive" },
        { "op": "Eq",        "field": "action_id", "value": "apply_proposal" },
        { "op": "IsNonNull", "field": "action_value" }
      ]
    },
    "consequent": "{\"route\":\"signal_arc\",\"signal\":\"proposal-applied\",\"correlate\":{\"proposal_id\":\"${entity.action_value}\"}}"
  },

  // Self-loop guard. Two checks: subtype-based (Slack tags bot posts
  // with subtype: "bot_message") and bot_id-based (any event with a
  // populated bot_id field). Either match drops the event.
  {
    "id": "ignore_bot_messages",
    "classification": "ignore",
    "antecedent": {
      "op": "Any",
      "args": [
        { "op": "Eq",        "field": "subtype", "value": "bot_message" },
        { "op": "IsNonNull", "field": "bot_id" }
      ]
    },
    "consequent": "ignore"
  }
]
```

### 7.3 Why correlation IDs ride `action_value`, not `action_id`

The earlier draft proposed encoding correlation values into `action_id`
(`apply_proposal:P-3`) and parsing the suffix in the routing template.
Routing templates only substitute fields verbatim — there is no
split/capture primitive. Solutions:

- **Project the value separately.** Block Kit elements carry a `value`
  field independent of `action_id`. Workflows compose buttons with
  `action_id: "apply_proposal"`, `value: "P-3"`. The sidecar projects
  `action.value` to the top-level `action_value` field. Routing
  templates substitute `${entity.action_value}` directly. Adopted.
- Add a regex-capture predicate to the routing engine. Daemon change;
  out of scope.
- Pass parsing concerns into the workflow itself via a `set_var` hook
  with a regex op. Hook-vocabulary change; out of scope.

The first path is free.

### 7.4 Channel-scoped and workspace-scoped routing

Match on `channel` (specific channel IDs) and `team_id` (workspace).
For multi-workspace installs, separate sidecar instances feed separate
webhook names (`slack-acme`, `slack-globex`); `team_id` matching is
defense in depth, not the primary scoping mechanism.

## 8. Outbound — `http_json` against api.slack.com

Workflow JSON composes Block Kit and POSTs directly:

```jsonc
{
  "op": "http_json",
  "args": {
    "method": "POST",
    "url": "https://slack.com/api/chat.postMessage",
    "headers": {
      "Authorization": "Bearer ${env.SLACK_BOT_TOKEN}",
      "Content-Type": "application/json; charset=utf-8"
    },
    "body": {
      "channel": "${vars.channel}",
      "thread_ts": "${vars.thread_ts}",
      "text": "Triage proposals ready — react to apply.",
      "blocks": [
        { "type": "section", "text": { "type": "mrkdwn", "text": "*P-3*: ${vars.p3_summary}" } },
        { "type": "actions", "elements": [
          { "type": "button",
            "text": { "type": "plain_text", "text": "Apply" },
            "action_id": "apply_proposal",
            "value": "P-3",
            "style": "primary" }
        ]}
      ]
    },
    "expect_status": [200]
  },
  "into_var": "post_response",
  "on_failure": "halt"
}
```

The shape (`op` / `args` / `into_var` / `on_failure`) matches `HookOp`
in `src/workflow/ops.rs`. Earlier doc drafts used `kind` at the top
level — that was wrong.

Workflow nodes can branch on `${vars.post_response.ok}` (Slack's API
returns 200 even on logical failures, with `ok: false` in the body).

### 8.1 Outbound endpoint inventory

Workflow `http_json` hooks against `slack.com/api/*`:

| Endpoint | Tier | Used for |
|---|---|---|
| `chat.postMessage` | v1 | Most outbound posts |
| `chat.update` | v1 | Edit a posted Block Kit message (e.g. mark proposal applied) |
| `chat.postEphemeral` | v1 | Per-user-only messages (insufficient-scope replies, etc.) |
| `views.open` | v1.5 | Open a modal (cosession kickoff, rejection-rationale capture) |
| `views.update` | v1.5 | Update a modal mid-flow |
| `views.publish` | v1.5 | Render / re-render the bot's App Home tab |
| `users.info` | v1.5 | Resolve user IDs for identity-claim modal |
| `conversations.create` | v1.5 | Create a council channel (§12.7) |
| `conversations.archive` | v1.5 | Close a council channel |
| `chat.getPermalink` | Phase II | Permalink for provenance citations |
| `files.info` + `url_private` download | Phase II | File ingestion |
| `conversations.history` / `.replies` | Phase II | Channel indexing |

Phase II additions:
- `files.info` + URL download (file ingestion)
- `conversations.history`, `conversations.replies` (channel indexing)

### 8.2 Block Kit ergonomics

Block Kit JSON inline in workflow vars works for v1 — keystone composes
similar payloads inline. If a real workflow proves it painful, the
escape hatches (block templates as vars, packet-driven block builders,
helper hook) can land later. v1 doesn't ship them.

### 8.3 429 handling

`exec_http_json` delegates to `HttpFetchResult { status, value }`
(`src/orchestration/http_fetch.rs`). Headers are NOT exposed; the
`retry-after` header value is unreachable from workflow nodes today.

The current workflow engine has no fixed-interval retry scheduler —
hook `on_failure` is one of `halt | warn | ignore`, and arc-level
back-edges are gated by a visit-count ceiling, not a delay
(`src/workflow/engine.rs:1128`). v1 behavior on 429:

- Workflow declares `expect_status: [200]`; a 429 fails the node.
- `on_failure: warn` lets the arc continue past the failure (post
  effectively dropped, surfaced via warning event).
- `on_failure: halt` terminates the arc with a `failed` outcome.
- Looping back to retry the post is possible only through a Wait node
  with a fixed timeout (the only delay primitive in the engine), and
  the visit-count ceiling caps how many cycles can fire.

Durable retry-after-aware backoff is v1.5. Two implementation paths:
extend `HttpFetchResult` to include selected headers (small daemon
change benefiting every workflow), or add a Slack-aware retry hook
(narrower but doesn't generalize). v1 accepts the gap honestly rather
than overpromising retry semantics that don't exist.

## 9. Authentication & secrets

Three Slack-issued credentials and one bbox-issued. Already laid out
in §2.2 — this section drills into scope inventory and rotation.

### 9.1 Token storage

| Secret | Format | Where | Used by |
|---|---|---|---|
| App-level token | `xapp-1-...` | Sidecar env: `SLACK_APP_TOKEN` | Sidecar — opens Socket Mode |
| Signing secret | hex string | Sidecar env: `SLACK_SIGNING_SECRET` | Sidecar (Events API path only) |
| Bot user token | `xoxb-...` | Daemon env only: `SLACK_BOT_TOKEN` | Workflow `http_json` hooks |
| Sidecar shared secret | random hex | Sidecar env + Daemon env: `BRO_SLACK_SHARED_SECRET` | HMAC on sidecar→daemon hop |

The sidecar does NOT hold the bot token. `self_user_id` and
`self_bot_id` are passed in via CLI flags (`--self-user-id`,
`--self-bot-id`), looked up out-of-band by the operator at app-install
time on api.slack.com and copied into the systemd unit. This keeps
§2.2's token-isolation claim honest: the daemon holds bot scopes; the
sidecar holds Socket Mode scopes; nothing crosses.

### 9.2 OAuth scope inventory by tier

**Sidecar-side (app-level token, Socket Mode):**
- `connections:write` — open Socket Mode connections (`apps.connections.open`)

**v1 bot scopes (bot token, OAuth-installed):**
- `app_mentions:read` — receive mention events
- `chat:write` — post messages
- `commands` — slash commands
- `reactions:read` — receive reaction events
- `users:read` — resolve user IDs in mention text (e.g. `<@U123>`
  rendered to display names in `/bbox status` output). Identity-claim
  flow that uses `users:read.email` is v1.5
- `channels:history` and/or `groups:history` and/or `im:history` and/or
  `mpim:history` — receive `message` events in the conversation types
  the bot operates in. Pick the minimum subset.

**v1.5 additions:**
- `chat:write.public` — post in channels the bot isn't a member of
  (only if needed; otherwise omit for least-privilege)
- `users:read.email` — resolve emails for the identity-claim modal

**Council channels (v1.5, §12.7):**
- `groups:write` — create + manage private channels via
  `conversations.create` and friends.
- `channels:manage` — only if public council channels are wanted.
- `chat:write.customize` — set per-persona display name + icon on
  council posts via `chat.postMessage`'s `username` / `icon_url`
  parameters.
- Without `groups:write`, the v1.5 flow falls back to running the
  council inline in the invoking channel.

**Phase II additions:**
- `files:read` — file ingestion
- `channels:history` (broader use) — full channel indexing for
  agentic-corpus

The example install (path TBD, lands with the implementation) will
ship a Slack app manifest declaring the v1 set as the default;
operators opt into v1.5 / Phase II by adding scopes and re-installing.

### 9.3 Token rotation

- **App token:** sidecar SIGTERM → systemd restart with new env. The
  restart gap costs no events: anything not yet acked is still Slack's
  to redeliver, and anything acked is in the durable spool (§5.6) and
  replays on the next start. Shutdown lets the delivery worker finish
  what it holds for ≤5s, and cutting that short only defers delivery.
- **Bot token:** daemon restart (Web API hooks read env at fire time).
  In-flight workflow nodes mid-POST may fail one call; retry policy
  handles it.
- **Sidecar shared secret:** rotate both processes simultaneously
  (systemd unit dependency).

## 10. Identity model

`slack:user:U01ABC` is stable within a workspace. Mapping to bbox
identity is needed for two reasons:
- Per-user scoping — apply a user-specific allow-list before a slash
  command can dispatch.
- Provenance — chain `commit` → `session` → `slack_message` →
  `slack_user` for the bbox_blame demo (Phase II).

### 10.1 Storage

`~/.bro/slack-identities.json`:

```jsonc
{
  "T01234567": {
    "U01ABC": { "bbox_user": "alice", "email": "alice@example.com", "scopes": ["all"] },
    "U02DEF": { "bbox_user": "bob",   "email": "bob@example.com",   "scopes": ["read"] }
  }
}
```

Manually authored at v1. A `/bbox claim` slash command (v1.5) writes
entries via a confirmation modal.

### 10.2 Default scope and dispatch gating

Unmapped users get `bbox_user: "anonymous"`, `bbox_scopes: ["read"]`,
`bbox_can_dispatch: false` (§6.3). The split between read-only and
dispatching commands is not "the same workflow runs but does less" —
it is **separate workflows gated at routing**. The routing packet
(§7.2) carries paired rule families:

- `start_arc → slack-bbox-command-dispatch` when
  `bbox_can_dispatch == true`.
- `start_arc → slack-bbox-command-readonly` when
  `bbox_can_dispatch == false`.

The read-only workflow handles `/bbox <question>`, `/bbox status`,
`/explain ...`. The dispatch workflow handles `/bbox triage`,
`/cosession start`, applying proposals. Routing decisions are made
before any workflow starts; an unauthorized command gets a polite
"insufficient scope" ephemeral reply from the read-only workflow.

Gating on the precomputed scalar `bbox_can_dispatch` instead of an
array-element check over `bbox_scopes` is deliberate — see §6.3 for
why scalar form is preferred when the routing engine already supports
it without quantified predicates.

## 11. Threading semantics

Slack `thread_ts` is the natural correlation key. Two concrete uses
plus one caveat the earlier draft elided.

### 11.1 Arc-per-thread

Each thread the bot participates in gets exactly one arc. The arc's
`Wait` nodes correlate on `{thread_ts: vars.thread_ts}`. Subsequent
events resume the arc.

**Caveat: the daemon currently has no Slack-thread arc registry.** If
two `app_mention` events arrive on the same thread before the first
arc has registered its first Wait, both fire `start_arc` and two arcs
race. The earlier draft proposed a `bbox_pin` setup-time guard, but
`bbox_pin` is set/list/delete with auto-generated IDs
(`src/pins.rs:168`) — not an atomic claim-if-absent primitive. Two
parallel workflows both observe "no pin," both create one, both
proceed.

v1 accepts the duplicate-arc risk as a known gap. The mitigation is
real but lives in v1.5: **daemon-side `start_arc` idempotency keyed
on `(workflow, correlate)`** — a new primitive, not Slack-specific,
useful for any webhook-driven workflow where duplicate triggers can
race. The Slack adapter's correlate key would be
`{channel, thread_ts}`. Persistent `WaitStore` (a separate v1.5 daemon
deliverable per [Workflow Engine](../../../docs/workflows.md)) is the right time to land both.

### 11.2 Reaction correlation: item_ts ≠ thread_ts in general

`reaction_added` carries `item.ts` — the timestamp of the message the
reaction was placed on. `item.ts` equals `thread_ts` ONLY when the
reaction is placed on the thread's parent message. A reaction on a
mid-thread reply has `item.ts != thread_ts`.

v1 design constraint: **reactions are attached to the bot's first
post in the thread**, which is the thread parent (or a message whose
ts equals the thread_ts of subsequent replies — same thing). The
workflow's outbound `chat.postMessage` becomes the thread root; the
arc's Wait correlates on that ts; reactions on that post resolve the
Wait.

For reactions on mid-thread messages, the sidecar would need to call
`conversations.replies` to fetch the parent thread_ts and project it
into the envelope. v1 does not implement this. Workflows that need
mid-thread reactions either constrain UX to bot-post reactions or
accept the limitation.

That one-sentence fix is not yet a specification, and four questions
have to be answered before anyone writes the code:

- **Which process holds the credential.** `conversations.replies`
  needs a bot token with the relevant `*:history` scope. §2.2 puts the
  bot token in the DAEMON and states as the honest security claim that
  the sidecar holds only the Socket Mode and signing credentials.
  Doing the lookup sidecar-side moves a bot token into the sidecar and
  invalidates that claim, so the alternative (enrich daemon-side, in
  the extractor or a routing pre-step, where the token already lives)
  has to be weighed rather than assumed away. §9.2's scope inventory
  also does not yet carry the history scopes for this purpose.
- **The envelope contract.** §6.1 defines no `parent_thread_ts` field,
  and §6 calls the envelope stable because routing packets depend on
  its shape. Adding a field, versus overwriting `thread_ts` with the
  parent, is a behavior fork: packets today read `thread_ts` as the
  reacted message's thread. Neither the field name nor the migration
  for existing packets is specified.
- **Failure and latency semantics.** The lookup is a rate-limited Web
  API call sitting on the INBOUND path, and since §5.6 the inbound
  path is ordered normalize, spool, ack. An enrichment call has to be
  placed relative to the durable write, and its failure mode has to be
  chosen: block the spool write, spool the unenriched envelope and
  enrich on replay, or drop. A reaction burst on one message also
  means N identical lookups, so a cache or coalescing policy is part
  of the answer.
- **The non-threaded case.** A reaction on a message that is not in a
  thread has no parent. Whether `parent_thread_ts` is then null or
  echoes `item_ts` changes what a correlating packet must match on.

Until those are settled, this is a protocol change, not an
implementation detail, and the v1 constraint above stands.

### 11.3 Channel-scoped badgey

For a per-channel badgey instance (badgey §10), the badgey's bro
session is `kind=work_item, tag=badgey-instance, target=slack:channel:<id>`.
Resume on every `app_mention` in the channel.

### 11.4 Bidirectional thread mirroring (Phase II)

Each Slack thread shadowed by a `bbox_thread(kind=investigation)` with
periodic message snapshots. Badgey can `bbox_search` against shadowed
threads. Depends on Slack entity refs in agentic-corpus (§12.10).

## 12. Capabilities

Tagged by tier from §4. Each subsection cites the daemon-side primitive
it depends on, and flags any new primitive it would need.

### 12.1 Outbound notifications [v1]

`http_json` POSTs to `chat.postMessage` from any workflow. Arc complete
→ post summary to a configured channel. Inbox digest → post to a DM
channel. No ingress required. Existing primitives.

### 12.2 Slash commands [v1]

`/bbox <text>` → `start_arc`. The arc's first node uses `response_url`
(now in the envelope per §6.1) for Slack's deferred-reply pattern;
result lands as a threaded reply.

Concrete commands shipped:
- `/bbox <question>` — badgey-ask
- `/bbox status [arc_id]` — `bro_arc_status` rendered as Block Kit
- `/bbox inbox` — personal `bbox_inbox` rendered as ephemeral Block Kit
- `/bbox triage` — kicks badgey-triage [v1.5; needs proposal flow]
- `/explain <entity_ref>` — narrated provenance
- `/cosession start` — opens a modal collecting provider + brofile + project + charter [v1.5]

### 12.3 App mentions [v1]

`@bbox <text>` in any channel the bot is in. Same handler shape as
slash commands; reply threaded; subsequent thread replies resume the
arc on the conversation.

### 12.4 Reactions as structured signals [v1, with caveats]

`domain:slack-reaction-routing` packet maps:
- `:white_check_mark:` → approve a proposal
- `:x:` → reject (rationale capture v1.5; needs modal flow per §12.5)
- `:eyes:` → bbox_pin "I'm reviewing" (status marker only — pin is
  not atomic, not an exclusive review claim, and concurrent reviewers
  each see the others' pins)
- `:apply-p3:` (custom emoji per proposal) → apply badgey proposal P-3

**v1 explicitly does NOT ship reaction-driven destructive verbs**
(`:stop_sign:` arc-cancel etc). Confirmation-window UX needs either
a stateful debounce node or a modal follow-up; both are v1.5 surfaces.

### 12.5 Block Kit approval messages [v1, rejection-rationale v1.5]

Producer-side proposals (badgey §6.5) ship as Block Kit messages.
Approve/Reject buttons fire `block_actions` payloads with
`action_value` carrying the proposal id; the routing packet signals
the arc.

Rejection-with-rationale needs the modal flow: `block_actions` opens
a modal via `views.open`; the user types rationale; `view_submission`
arrives with `view_state_values` populated. The sidecar projects
`view_state_values` (§6.2). The arc waiting on the rejection signal
sees the projection as `${last_signal.payload.view_state_values}` at
resume time (signal payloads land via `last_signal`, not `entity` —
`entity` is the routing-time substitution, not a workflow-runtime
variable; see [Workflow Engine](../../../docs/workflows.md) signal substitution). v1.5.

### 12.6 App Home as personal dashboard [v1.5]

`app_home_opened` fires; workflow renders the user's `bbox_inbox` view
as Block Kit and posts via `views.publish` (Slack's Home tab API —
both initial render and subsequent updates use `views.publish`;
`views.update` is for modal updates only). Slack does not require a
specific OAuth scope for `views.publish` against the bot's own Home,
but the app must declare the Home tab as enabled in the app
configuration / manifest. Document this in the example install.

Synchronous render at v1.5; if computation gets slow, fall back to a
"loading…" placeholder + async re-publish when ready.

### 12.7 Council channels [v1.5]

Originally proposed as v1; demoted on round-2 review for two
honest reasons:

1. **Streaming is not free.** Council deliberations stream as posts
   land. `http_json` cannot consume SSE from `/council/{id}/tail`,
   and a polling loop on `bro_council_posts` via the `mcp_call` op
   (`src/workflow/ops.rs:76`) is mechanically possible but requires
   loop-with-Wait scheduling (Wait is the only delay primitive) and
   a per-iteration cursor — workable but not a single-node hook.
2. **Persona voice is harder than IRC's.** `bro-irc` spawns one IRC
   client per council member with distinct nicks
   (`src/irc_bridge.rs:705`), giving each persona its own visual
   identity. Slack's one-bot-per-app constraint forces either a
   persona-prefix-in-body workaround (`*[reviewer-a]* ...` — visually
   poor) or one-app-per-persona (OAuth maintenance per persona —
   heavy). Neither is a one-hour design-doc-to-ship gap.

The v1.5 sketch:
- `/council fix-flaky-build` creates a private channel via
  `conversations.create` (requires `groups:write`).
- Workflow spawns a council via
  `http_json POST http://127.0.0.1:7264/council`.
- A polling loop alternates `mcp_call: bro_council_posts(since_seq=N)`
  (the actual MCP arg name per `src/main.rs:3942`) with a short-Wait
  node; new posts get forwarded as threaded replies via
  `chat.postMessage` with `username` + `icon_url` overrides
  (`chat:write.customize` scope; Slack treats these as user-visible
  customization that operators should signpost in app config) for
  per-persona display.
- Channel closes by archiving via `conversations.archive` AND closing
  the council via `http_json DELETE http://127.0.0.1:7264/council/{id}`.

Skeleton (illustrative — exact node graph is a v1.5 example):

```
PollPosts (Noop)
  on_enter:
    - op: mcp_call
      args: { server: "blackbox", tool: "bro_council_posts",
              arguments: { id: "${vars.council_id}", since_seq: "${vars.cursor}" } }
      into_var: posts_batch
    # cursor advances to the highest sequence in the returned batch;
    # response shape is { council_id, posts: [{sequence, ...}, ...] }
    - op: find_first   # used to pick the max-sequence post; iteration
                       # over batch entries to forward as Slack posts
                       # is via workflow back-edge, not engine foreach
      args: { ... }
    - op: http_json   # chat.postMessage per new post in batch
      args: { ... }
  next: WaitTick

WaitTick (Wait)
  any_of: [council-closed]
  timeout: 5s
  on_timeout:  goto: PollPosts
  on_signal:   goto: Closeup
```

The Wait-with-timeout-back-edge is the canonical bbox idiom for delay
scheduling — the same pattern keystone uses for PR-feedback waits
with timeouts.

Without `groups:write`, fall back to running the council inline in the
invoking channel via threaded replies — same delivery mechanism, no
scope escalation.

v1 ships only `/bbox` slash commands, app mentions, reactions, and
Block Kit approvals. Council on Slack waits for the polling-loop
pattern to exist as a documented v1.5 example.

### 12.8 File ingestion [Phase II]

`file_shared` triggers a workflow that fetches via `files.info` and
the file's `url_private` (Bearer auth), then routes the bytes through
the agentic-corpus chunker registry (§7.3 in `agentic-corpus.md`).

This is **not v1.** The chunker registry and `project_file` chunk
ingestion are agentic-corpus deliverables; the current `http_json`
hook can fetch a URL but cannot pipe bytes into the chunker
pipeline. v1 receives `file_shared` events but routes them to
`ignore` until the agentic-corpus integration lands.

Replayability note: Slack's `url_private` URLs require the bot token
at fetch time and **expire** when the file is deleted from the
workspace. Phase II ingestion must download + store at event time;
re-fetching at index-rebuild time is not reliable. The Phase II
design needs to decide whether to mirror file bytes locally (storage
cost) or accept that some chunks become unreadable on workspace-side
deletion.

### 12.9 Permalink-anchored provenance [Phase II]

`chat.getPermalink` provides stable URLs for messages. Knowledge
entries written from Slack-driven decisions carry `derived_from`
references; `bbox_blame` walks `file:line` → `commit` → `session`
→ `slack_message` → permalink.

Depends on Slack entity refs in agentic-corpus (§12.10). Not v1.

### 12.10 Slack as first-class entity in agentic-corpus [Phase II]

Ownership note (2026-08-11): channel indexing, ingestion, and
permalink-anchored provenance are now owned by
`design/connectors/slack-ingestion-connector.md`, which re-homes them onto
the producer-plane connector architecture and retires the
`/bbox index-channel` per-channel opt-in below (agent- or user-triggered
mutation of corpus scope is disallowed by the onboarding trust model). The
entity grammar sketch remains salvage input to the graph-projection phase of
the connector program.

Extend the entity-ref grammar:

```
slack_message:<workspace>:<channel>:<ts>
slack_user:<workspace>:<user_id>
slack_channel:<workspace>:<channel_id>
slack_thread:<workspace>:<channel>:<thread_ts>
```

Edges: `IN_THREAD`, `BY_USER`, `IN_CHANNEL`, `MENTIONED_USER`,
`REACTION_BY_USER`, `LINKS_TO_PERMALINK`, `DERIVED_FROM_SLACK`. Index
channel history into `transcript`-style docs. Privacy gate: only
channels the bot is a member of are eligible; per-channel opt-in via
`/bbox index-channel` rather than auto-on.

Owned by a separate ingestion arc that pulls `conversations.replies`
on a schedule and writes deltas. Channel-removal triggers index
purge. Phase II.

## 13. Observability

### 13.1 Sidecar health

Sidecar exposes a small `/health` endpoint on a configurable loopback
port (off by default; set `--health-port=7299` to enable):

```json
{
  "connected": true,
  "uptime_secs": 3712,
  "last_event_at": "2026-05-05T12:34:50Z",
  "events_forwarded": 1284,
  "events_dropped_self_loop": 47,
  "events_failed_post": 2,
  "events_failed_post_exhausted": 0,
  "events_spooled": 1284,
  "events_spool_replayed": 3,
  "events_spool_write_failed": 0,
  "events_spool_discarded_aged": 0,
  "events_spool_evicted_overflow": 0,
  "spool_depth": 0,
  "ack_latency_ms_p50": 14,
  "daemon_post_latency_ms_p50": 6,
  "reconnects": 1,
  "last_disconnect_reason": null,
  "rate_limited_events": 0,
  "self_user_id": "U0BOTUSR0",
  "self_bot_id": "B0BOTBOT0",
  "workspace_id": "T01234567"
}
```

### 13.2 Daemon-side surfaces

All existing webhook tools apply:
- `bro_webhook_deliveries(name="slack")` — extracted entity + routing
  verdict + status + response body. The full inbound body is preserved
  only via the extractor (`raw_event` projection in §6.5) — what's not
  projected isn't queryable through this tool. For raw inbound capture,
  rely on sidecar `tracing` logs or the v1.5 disk buffer (§14).
- `bro_webhook_replay(name="slack", body=..., headers=...)` — iterate
  routing rules against synthetic envelopes.
- `bro_arc_status` — arcs that started from Slack.
- `bro_signals(signal=...)` — signals matched vs idle.

No new MCP tool. Slack-specific debugging happens through the generic
webhook tools by filtering `name="slack"`.

### 13.3 Structured logging

Sidecar logs to stderr in `tracing`-compatible JSON. Daemon logs
received webhooks. Cross-process correlation key: `envelope_id`.

## 14. Failure modes & known gaps

| Failure / gap | v1 behavior | Future fix |
|---|---|---|
| Slack down | Sidecar reconnect loop; daemon untouched | — |
| Sidecar crash | systemd restart; pre-ack events redeliver from Slack; post-ack events are in the durable spool (§5.6) and replay on start | — |
| Daemon down | Envelope is spooled and acked, the worker's retry budget fails, the entry is RETAINED and re-attempted by the boot replay and the 300s sweep until 2xx or the 24h age bound. After 3 consecutive failed rounds the endpoint breaker gates attempts so a large spool does not grind | — |
| Daemon crashes between returning 2xx and the sidecar deleting the entry | The entry replays on the next pass and the daemon sees the envelope twice. This is the at-least-once ceiling and cannot be closed sidecar-side | Daemon-side `X-Slack-Envelope-Id` dedupe hardening (tracked separately) |
| Sidecar cannot write its spool (disk full, permissions) | Ack is WITHHELD so Slack redelivers; `events_spool_write_failed` climbs and the error log names the spool dir. Nothing is lost, but the sidecar is not durably accepting traffic and needs an operator | Health-endpoint alerting |
| Daemon down longer than the spool age bound | Entries past `--spool-max-age-secs` are discarded with a structured error naming each envelope_id; `events_spool_discarded_aged` counts them | Longer bound, or a dead-letter export |
| Token rotated mid-flight | 401; sidecar exits and systemd restarts | — |
| Outbound 429 | Workflow node fails; engine has no fixed-interval retry — `on_failure: warn` lets the arc continue past the failed post; `on_failure: halt` terminates the arc | Expose response headers in `HttpFetchResult` (v1.5); Slack-aware retry hook |
| `WaitStore` is in-memory ([Workflow Engine](../../../docs/workflows.md) known limitation) | Daemon restart loses every suspended arc; Slack events arriving while the arc is gone correlate to nothing | Disk-backed WaitStore (v1.5 — daemon-wide phase-next, not Slack-specific) |
| Two app_mentions on same thread before Wait registers | Both fire start_arc; two arcs race. `bbox_pin` is not atomic so cannot guard | Daemon-side `start_arc` idempotency keyed on `(workflow, correlate)` at v1.5 |
| Reaction on mid-thread message | `item_ts != thread_ts`; correlation misses | Unresolved: the named fix (sidecar `conversations.replies` lookup) contradicts §2.2 token isolation and is underspecified. See §11.2 |
| Slack Workflow Builder posting to webhook | Not supported at v1 (loopback-only daemon cannot receive cloud Slack POSTs) | Public-ingress deployment shape (out of v1 scope) |
| Bot kicked from channel | Channel-scoped events stop; arcs correlated to that channel's threads time out per Wait policies | — |

The contract is bounded AT-LEAST-ONCE delivery to the daemon: an
envelope the sidecar has acked is on local disk and is retried until
the daemon returns 2xx or the entry passes `--spool-max-age-secs`.

At-least-once is the honest ceiling, and the sidecar cannot raise it.
Delivery and spool deletion are two operations with no atomic bracket
around them, so a crash between the daemon's 2xx and the delete leaves
an entry that is replayed on the next start. Suppressing that
duplicate is the DAEMON's job, via `X-Slack-Envelope-Id` dedup, and
whatever guarantee the combined system offers is a property of that
dedup, not of this spool. Do not read the spool as exactly-once at any
layer; hardening the daemon-side dedup is tracked separately.

What the sidecar cannot promise at all is anything downstream of the
POST: once the daemon accepts an envelope, persistence is whatever the
workflow engine offers.

## 15. Boundaries — what `bro-slack` is NOT

- **NOT a router.** Routing is a daemon responsibility expressed in
  packets. The sidecar carries no business logic.
- **NOT a workflow engine.** No retry, no state, no correlation. Ack
  semantics aside, one POST per inbound event; whatever the daemon
  does with it is the daemon's problem.
- **NOT a Slack-specific module in the daemon.** The daemon does not
  link slack crates, does not parse Slack envelopes natively, does not
  carry Slack-specific signature schemes.
- **NOT bound to one Slack workspace.** A single sidecar instance
  serves one workspace; multiple workspaces run multiple sidecars on
  different `--webhook-name`s. Each maps to a separate webhook spec.
- **NOT a token broker.** Each process reads its own env. The "shared
  secret HMAC" mode is not token brokerage; it's a transport-layer
  authentication shared between two trusted processes.
- **NOT exposed as MCP tools.** No Slack-aware MCP surface. Existing
  `bro_webhook_*` tools work for Slack the same as for Forgejo.
- **NOT an ingestion arc.** File and channel-history ingestion are
  Phase II workflows that fire `http_json` and route through the
  agentic-corpus chunker registry; the sidecar pulls nothing.

## 16. Non-goals

- **Enterprise Grid multi-team routing.** v1 single-team; multi-workspace
  is multiple sidecars.
- **Full OAuth onboarding flow.** v1 is "operator installs the app to
  their workspace, copies tokens to env." Not a hosted-bbox flow.
- **Slack Marketplace publishing.** Not a goal.
- **DLP / data-residency.** No content filtering inbound or outbound.
- **End-to-end encryption.** Not a Slack property; if needed, run Matrix.
- **Bot-as-multiple-personas with distinct user identities.** Slack's
  one-bot-per-app constraint; persona prefix in body is the v1 answer.
- **Replacing the IRC bridge.** Both can run; both feed the same
  daemon over distinct daemon-side contracts (`/irc/*` typed RPCs vs
  `/webhook/<name>` envelope routing).

## 17. Sidecar pattern as future-proofing — not yet a template

The sidecar design is intended to make a future Discord/Matrix/
Mattermost/Zulip adapter cheaper, but the doc deliberately avoids
publishing a per-platform mapping table until a second adapter ships.
The IRC bridge is not a counterexample — it solves a different problem
(typed command RPCs) and uses a different daemon contract. Claiming
"adapter parity" before the pattern has been instantiated twice is
premature.

The principle worth committing to: **the daemon's `/webhook/<name>`
envelope contract is platform-agnostic; the sidecar's job is to
project a platform's events into that contract**. That principle stays
true whether or not a second adapter ever lands. If one does, the
shared bits — process supervision, ack semantics, identity enrichment,
reconnect/backoff — naturally factor into a `bro-bridge-core` library
crate at that point. Not before.

Zulip, sometimes cited as a closer model for bbox threads because of
its streams-and-topics structure, deserves a footnote: Zulip topics
are good correlation keys (one topic = one logical conversation), but
they are not the same object as bbox threads. Bbox threads carry kind
(investigation vs work_item), status, notes, edges, durable lifecycle.
A Zulip topic is a stream-local conversation label. Strong shape match
on threading; not equivalence.

## 18. Open design questions

Items that are real toss-ups, not biases. Resolved decisions live in
the design body above.

1. **Daemon-side outbound rate limiting.** v1 accepts 429s and lets
   workflow retry policy handle it without `retry-after` awareness.
   Should v1.5 add headers to `HttpFetchResult` (small daemon change,
   benefits everything that uses `http_json`) or a Slack-aware wrapper
   (Slack-specific, doesn't generalize)? The headers path is more
   reusable but touches a daemon contract used by every workflow.

2. **Channel history ingestion privacy default.** Phase II indexes
   channel messages into agentic-corpus. Default-on for bot-member
   channels, or default-off with explicit opt-in? Bias has been
   "explicit opt-in via `/bbox index-channel`"; the question is
   whether that's still right when a workspace admin installs the
   app expecting full transparency.

3. **One-app-per-persona vs persona-prefix-in-body.** Persona prefix
   ships at v1; one-app-per-persona is cleaner but compounds OAuth
   maintenance per persona. Worth revisiting after councils get real
   usage.

4. **`bro-irc` and `bro-slack` shared core.** Both run as systemd-
   supervised sidecars; both reconnect on transport failure; both
   forward to the daemon. Worth factoring a `bro-bridge-core` crate
   now (one consumer + speculative second consumer), or wait until a
   third sidecar exists?

5. **Per-user dispatch authorization for unmapped users.** Read-only
   default is the v1 answer. Should unmapped users get *any* surface,
   or block entirely until claimed? Bias is read-only;
   counter-argument is that anonymous read access in shared workspaces
   leaks search results across team boundaries.

6. **Sidecar disk buffer for daemon-down windows.** Today, post-ack
   events are lost when the daemon is down past the 3-retry budget.
   v1 accepts this; v1.5 disk-buffer is on the table. Buffer storage
   shape (jsonl, sqlite, append-only WAL) is unspecified; complexity
   non-trivial.

7. **Permalink as canonical entity-ref render form.** When emitting
   `slack_message:T1:C2:1730816096.000300` in citations, render as
   the structured form or as the human permalink? Bias is
   structured-canonical, permalink-as-render-time-alias.

8. **Slack-specific routing-template capture.** `${entity.action_value}`
   suffices for v1 because action_id stays static and value carries
   the dynamic part. If future Slack integrations need to parse
   structured patterns out of `text` or `block_id`, a regex-capture
   primitive on the routing engine becomes attractive. Daemon change;
   not Slack-specific in scope.

## 19. Glossary

- **bro-slack** — sidecar binary holding Socket Mode WebSocket and
  forwarding normalized envelopes to `blackboxd`.
- **Sidecar pattern** — per-platform transport adapter living as a
  separate process; daemon stays platform-unaware.
- **Webhook envelope** — the normalized JSON shape (§6.1) emitted by
  the sidecar to the daemon, projecting the most-used Slack fields to
  top level with full payload preserved under `raw`.
- **Socket Mode** — Slack's WebSocket-based event delivery mechanism;
  alternative to the public-ingress Events API. No public ingress
  required.
- **Block Kit** — Slack's interactive message UI primitive; JSON
  describing layout, buttons, modals.
- **`thread_ts`** — Slack's parent-message timestamp identifying a
  thread; the natural correlation key for bbox `Wait` nodes.
- **`item_ts`** — for reaction events, the timestamp of the reacted-to
  message. Equals `thread_ts` only when the reaction is on the thread
  parent.
- **`team_id`** — Slack workspace identifier.
- **Self-loop** — the bot's own posts surfacing as `message` events;
  filtered at sidecar (by `bot_id` AND `user_id`) and at routing
  packet (by `subtype` / `bot_id`).
- **Council channel** — a private Slack channel auto-created by
  `/council`, hosting a multi-agent deliberation tied to a council on
  the daemon's existing `/council/*` HTTP routes.
- **App-level token** (`xapp-`) — Socket Mode connection auth.
- **Bot user token** (`xoxb-`) — Web API auth for outbound posts.
- **Signing secret** — shared secret for verifying inbound HTTP Events
  API signatures; unused on the Socket Mode path.
- **ACL enrichment** — sidecar-side projection of identity-file lookup
  into `_meta.bbox_user` / `_meta.bbox_scopes` so routing packets can
  gate on it without reading external files.

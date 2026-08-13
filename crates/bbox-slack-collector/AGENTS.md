# bbox-slack-collector

The conversation satellite: a producer-host binary that observes a Slack
workspace from the bot's own perspective and publishes message records over the
`/internal/conversation-source/v1/*` wire. Sibling of `bbox-file-collector` (the
file-tree profile) and of the `bro-slack` bridge (which owns interaction; this
owns observation). Design:
`design/connectors/slack-ingestion-connector.md`.

## The invariant that outranks everything else here

**Write-safety is a property of the CODE PATH, not of the credential.** The
deployed posture is ONE Slack app, the operator's existing interactive bot
(design 3.1, ruled 2026-08-13), because the requirement is observation from the
bot's own membership. A Slack app holds one bot token per install carrying all
its granted scopes, so there is no read-only credential to hold and no
"this grant carries no write scope" assertion to make.

Three layers replace it, and all three must survive every future change:

1. **No write call sites.** Nothing in this crate composes a mutation.
2. **A closed read-method enum.** `slack::SlackReadMethod` is the only thing the
   client builds a request path from. There is no string-taking entry point.
   `SlackReadMethod::parse` exists for diagnostics and refuses everything
   outside the set.
3. **The dependency ceiling**, `scripts/acceptance-slack-collector-deps.sh`.

**Never add a Slack SDK.** That is not a style preference. An SDK hands every
future call site `chat.postMessage` as an ordinary function and layer two decays
from a construction into a convention. The client is `reqwest` plus the enum for
exactly this reason, and the acceptance script names the popular SDKs
explicitly.

The collector cannot refuse a write scope under this posture, so it RECORDS the
granted set on every cycle and surfaces the write subset separately. It does
refuse a MISSING read scope, because that failure otherwise presents as a
healthy satellite over an empty corpus.

## Footguns, in rough order of how expensive they are to rediscover

**Never land an incomplete window.** `conversations.history` pages NEWEST first.
A sweep that exhausted its page budget holds the newest part of its range, not a
contiguous run from the watermark, so landing it advances the cursor past
messages nothing will ever come back for. `cycle.rs` discards incomplete windows
and counts them as `windows_deferred`. A nonzero count means the channel is
busier than one window's page budget; the fix is a SMALLER `sweep.window_secs`,
not a bigger page budget.

**Both sweep bounds are inclusive and the caller filters the lower one.** Slack's
`inclusive` flag covers `oldest` and `latest` together, so exclusive bounds drop
a message whose `ts` falls exactly on a window edge: excluded from the window
ending there and from the window starting there. Inclusive plus a caller-side
filter on the resume mark lands every message exactly once.

**Thread replies are not an enrichment, they are most of the conversation.**
Unbroadcast replies never appear in a channel history sweep. The cheap
latest-reply test only works while the parent is still inside a window being
re-read, so known threads also come around on a bounded ROTATION
(`ThreadMark::last_swept_cycle`). Remove the rotation and an old thread's new
reply becomes invisible forever.

**Reply baselines are excluded from the tombstone diff.** A history resweep does
not return unbroadcast replies, so diffing them for absence would tombstone every
thread reply in the window on the first reconciliation pass.
`MessageBaseline::thread_reply` exists solely to prevent that, and the resulting
limit (deletion detection covers channel-visible messages only) is narrower than
design 5.4's window bound and is stated rather than assumed.

**`ok: false` arrives with HTTP 200.** Slack's error channel is in the body. A
client that trusts the status line reads `{"ok":false,"error":"ratelimited"}` as
a successful empty page and advances a watermark over messages it never saw.

**The journal is losable; the cursor is not.** The server owns the cursor. Every
journal mark advances only AFTER a receipt, and the resume point is
`max(server watermark, local mark)`. Reverse that comparison and a stale local
mark skips messages the corpus never received. Deleting the journal costs edit
and delete detection for one window plus some redundant sweeps, never data.

**The rate budget is shared with a live human-facing process.** The interactive
bot draws on the same credential from another process. The pacer is a
minimum-interval gate rather than a refilling bucket precisely because a bucket
permits a burst, and a burst steals headroom at the moment a human is waiting.
The cross-process token bucket is later work; until it exists, being a polite
minority consumer (default 20 rpm against a ~50 rpm band) is the whole strategy.

**An empty `channels.include` enrolls NOTHING.** Deliberately inverted from the
file lane, where empty means "everything the other rules allow". Design section 8
is explicit that there is no index-everything posture here.

## Testing

`cargo nextest run --workspace -E 'package(bbox-slack-collector)'`.

The integration tests drive the REAL Slack client against a fixture HTTP server
(`tests/support`), because pagination, `ok:false`, and 429 handling are transport
behaviors a hand-written double would only prove to itself. The corpus side is a
MODEL of the landing store rather than the real one: the dependency ceiling
forbids `bbox-conversation-source-store`, so the model is written from the wire
crate's types and reproduces only what the cycle depends on (idempotent landing,
server-derived cursors, receipts whose counts partition the request).

Repo test-isolation invariants apply: canonicalize tempdir roots before path
assertions, never touch real `$HOME`, never open a socket to anything but a
fixture server bound on loopback.

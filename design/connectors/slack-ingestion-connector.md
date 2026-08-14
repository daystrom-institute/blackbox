---
title: "Slack Ingestion Connector"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - connectors
  - slack
tags:
  - connectors
  - integrations
  - slack
  - locality
  - producer
  - transcripts
  - ingestion
brief: "Index visible Slack messages as a searchable conversation corpus through a read-scoped producer satellite. Slack is the conversation profile of the connector family and mechanically an API-dataset observation: messages ride an append-only ingest lane with per-channel cursors, not the code-source manifest lane. Complement of the bro-slack agent bridge, which keeps interaction; this doc owns observation only."
date: 2026-08-11
---

# Slack Ingestion Connector

Status (2026-08-11): proposed, nothing implemented. The transports this design
rides on are real: the authenticated producer wire, `ServiceToken` producer
grants, and two-sided operator-config onboarding all shipped with the
code-source collector. The conversation ingest lane described here does not
exist. The transcript adapter registry today reads local provider session files
only; there is no Slack source, endpoint family, or producer binary. The
`bro-slack` bridge is design (archived v1, proposed next), not deployed code.
Reverify against the `bbox-corpus-index` transcripts module and the code-source
server routes before treating any contract name here as landed.

## 1. What this connector observes

Slack is the **conversation profile** of the connector family, and
mechanically an **API-dataset observation**: unlike the file-tree profile, the
source is not a mutable tree of named blobs but an append-only event log behind
a paginated, rate-limited API. Its shape:

- Messages are conversational turns with an author, a monotonic `ts`, and an
  optional thread parent. They are appended, not rewritten in place.
- There is no whole-set digest. No cheap query answers "what is this channel's
  current content"; you page through history.
- Edits and deletions are out-of-band revisions to already-observed records.
  An edited message keeps its `ts` and gains an `edited` sub-object; a deleted
  message simply stops appearing.
- Visibility is a property of the token, not of a path: what the connector can
  read is exactly what the installed app's grant plus channel membership allow.

That profile is transcript-shaped. It is not project-file-shaped.

## 2. Central decision: the conversation ingest lane, not the manifest lane

**Slack messages ride an append-only conversation ingest lane with per-channel
cursors and explicit revision records. They do not ride the code-source
manifest lane.**

The corpus plane already draws this line for its own sources: code shipping is
current-state with deletes falling out of a manifest diff, transcript shipping
is append-only with cursors. The locality design says so directly, and the
collector's non-goals name `/internal/records` as out of scope precisely
because the two shapes do not share an endpoint.

Why the manifest lane misfits Slack:

- **Deletion-falls-out-of-the-diff is bought with full enumeration.** The
  manifest lane gets exact deletes because the producer re-walks the whole tree
  every cycle. A filesystem walk is cheap; a full re-enumeration of channel
  history is a paginated sweep against a per-workspace rate budget, every
  cycle, to find a delta a `ts` watermark finds in one call. The lane's central
  bargain inverts from cheap to prohibitive.
- **Generations are the wrong quantum.** A code generation is a coherent
  snapshot pinned to a HEAD and a dirty fingerprint, activated stage-then-flip.
  A channel has no snapshot boundary and no reason to hide new messages until a
  generation completes. Conversation ingest wants monotonic per-channel
  advancement.
- **The cursor vocabulary already exists.** `TranscriptCursor` carries
  `ProviderEventId` and `MessageIdSet` variants alongside byte offsets, which
  is what a `ts` watermark and a set of in-flight thread parents need.
- **Message granularity is load-bearing.** A tombstone applies to one message.
  A file-granular lane can only say "this file changed", which forces the
  producer to choose message-to-file bucketing, which is a document-shaping
  decision, which belongs corpus-side.

### Rejected alternative: shoehorn channels into project-file trees

The shortcut is to render each channel-day into a synthetic file
(`#channel/2026-08-11.md`), ship it through the existing manifest lane, and
inherit identity, activation, purge, and search for free. Rejected because it
pays the full-enumeration cost above every cycle (the digest is over the whole
set); fabricates a repo-shaped identity for something with no repository, which
does not dissolve the scope-minting problem but records it as a lie in the
catalog; moves day-bucketing and message rendering, both chunking-adjacent,
into the producer in violation of the enforced no-chunker-in-the-producer
invariant; loses per-message tombstones, so the corpus cannot distinguish a
redaction from an append; and pollutes code search with pseudo-files the file
classifier, active-selector constraint, and every code-facing reader must
special-case forever.

### Two more rejected shapes

**The daemon polls Slack directly.** Smallest new machinery, wrong plane. It
puts a workspace-wide credential inside the cage daemon, gives the corpus plane
an outbound dependency on a third-party API, and makes ingest availability a
daemon-availability concern. Producer-plane observation keeps the credential on
the producer host and the corpus host a receiver, which is the point of the
collector template.

**The bridge's live event stream as sole authority.** The `bro-slack` sidecar
already receives every message event over Socket Mode, but indexing straight
off it is wrong as the *only* source: no backfill, dropped events across
disconnects and restarts, and ack semantics targeting interaction latency
rather than corpus completeness. It survives in a strictly subordinate role
(section 5.4) as a low-latency dirty-marking hint.

## 3. Producer: a read-scoped Slack satellite

### 3.1 Sibling binary, shared credential host

The producer is `bbox-slack-collector`: a read-only satellite that runs on a
producer host, holds the Slack read credential, and publishes to the corpus
host over the authenticated internal wire. It is **a sibling of the `bro-slack`
bridge, not an extension of it**, deployed on the same credential host.

Folding ingestion into the bridge process (one process, one app, one reconnect
loop) was considered and rejected:

- **Scope auditability.** The bridge needs interactive write scopes
  (`chat:write`, `views:*`, eventually channel creation); the reader needs
  `channels:history`, `groups:history`, `users:read`, later `files:read`.
  Merged into one grant, nobody can read the workspace's installed-app page and
  tell which capability serves which purpose, or revoke one without the other.
- **A read-only credential is mechanically enforceable.** With its own token,
  the producer asserts at startup that its grant carries no write scope and
  refuses to run otherwise, making the operator's standing no-writes-to-Slack
  rule a property of the credential rather than of the code path. That
  assertion is impossible in a process that also holds the posting token.
- **Rate-limit isolation.** A multi-hour backfill and an interactive mention
  response should not draw on the same bucket.

**RULED (operator, 2026-08-13): the deployed posture is ONE app, the
existing interactive bot.** The requirement is observation from the bot's own
perspective: the bot's channel membership defines exactly what it indexes, so
the bot can search its own history when directed by humans. A Slack app holds
one bot token per install carrying all granted scopes, so a read-only
credential for the same bot identity does not exist; credential-level
enforcement is therefore unavailable, and write-safety moves to the collector's
code path as a threefold contract: the collector has no write call sites, its
Slack client is allowlisted to the read API families (conversations.history,
conversations.replies, users.*, and cursor pagination) and refuses any other
method by construction, and the dependency ceiling is enforced by acceptance
script. The agents-never-post rule is untouched: it binds agents, and the
collector is an observer that structurally cannot compose a write. The
two-app split remains documented below as the posture for deployments that
want credential-level enforcement and separable workspace audit; it is not
the deployed shape here.

Consequence of one app: the interactive bot and the collector share one rate
budget, so the S3 workspace token bucket must span both processes, and until
it exists the collector self-throttles conservatively and yields to
interactive traffic. Steady-state observation remains watermark polling, not
the bot's socket-mode event stream: events miss everything during downtime
while polling self-heals; event-assisted freshness (the bridge nudging the
collector) is a later optimization. The first consuming deployment resolves
the bot token from the operator's secret vault via the op CLI at startup
(a secret reference, never a literal in config), per the secrets-provider
design.

Original recommendation, retained for deployments without the
bot-perspective requirement: **two Slack apps**, the existing interactive app
and a read-only observer app. Where an operator insists on one app without
the bot-perspective requirement, the fallback is two token files with
distinct grants and the same startup assertion, which is supported but weaker
because the workspace-level audit surface no longer separates the purposes. Both processes share the producer host and therefore
share credential delivery, supervision, and the operator's secret plane
([`../operations/config-artifacts/secrets-provider.md`](../operations/config-artifacts/secrets-provider.md)).

### 3.2 Producer discipline

The producer ships records, not documents. It normalizes Slack JSON into a
bounded record shape (author id, `ts`, thread parent, raw text, structural
fields, reaction and attachment references) and stops. It does not concatenate
messages into synthetic documents, choose thread windows, summarize, render
markdown, or embed. Every document-shaping decision stays corpus-side, so
exactly one chunker version exists in the system and a producer deploy cannot
skew against the index.

The producer's dependency tree is guarded the way the code collector's is: an
acceptance script over the resolved graph rejecting Tantivy, the chunker,
corpus-index, indexing, embedding, vector, edge-index, the root package, and
the harness.

## 4. Wire, auth, and identity

### 4.1 Transport

A dedicated authenticated endpoint family, mounted beside the code-source
routes and never reachable from model or shell authority:

```text
POST /internal/conversation-source/v1/channels     roster visible under policy
GET  /internal/conversation-source/v1/cursors      server's per-channel high-water marks
POST /internal/conversation-source/v1/batches      ordered batch of new message records
POST /internal/conversation-source/v1/revisions    edits and tombstones for landed records
GET  /internal/conversation-source/v1/status       per-channel lag, last batch, tombstone counts
```

The server owns the cursor. The producer asks where to resume rather than
asserting where it left off, mirroring the collector's "server is the authority
on what it still needs" invariant and making producer restarts, reinstalls, and
host moves recoverable with no producer-side durable state. There is no spool:
Slack is the durable backlog, and a resweep from an older watermark is always
safe because landing is idempotent on
`(workspace_id, channel_id, message_ts, revision)`: workspace-level
conversation identity, not per-scope identity, so two sources observing the
same channel converge on one document (section 4.2).

Auth is the collector's, unchanged: bearer `ServiceToken` with its owner, mode,
symlink, and shape checks; the header authenticates a producer before the
bounded body is parsed; the body's scope must be an exact member of that
producer's server-side allowlist before any request data enters durable state;
tokens never appear in query strings, bodies, environment variables, MCP
arguments, logs, or metrics; non-loopback plain HTTP is refused producer-side
and redirect following is off.

### 4.2 Identity, and the inherited open question

A Slack workspace has no committed `.bbox/config.toml` and therefore no
`repo_id`. `PublishedScope` does not describe it. Scope minting for non-git
sources is the connector family's shared open question, analyzed in
[`remote-source-connectors.md`](remote-source-connectors.md); this doc adopts
whatever that settles on rather than duplicating the analysis.

Independent of the outcome:

- Durable conversation identity is `(workspace_id, channel_id)`, both
  Slack-issued opaque ids.
- Channel names, workspace domains, and display names are **attachment
  observations, never identity**. Channels and users get renamed; ids do not.
  This is the discipline the project catalog applies to absolute paths.
- The scope is minted at grant time and recorded in the producer grant. A
  request can never introduce a scope the operator did not already write into
  the corpus host's config.

**Disjoint observers, one workspace (an intended use case, not an edge
case).** Two bot identities installed in the same workspace are two sources:
each gets its own minted scope, its own token, its own producer grant, and a
visibility bound defined by its own grant plus channel membership. They are
indexed separately and searchable through the same corpus instance. Identity
converges beneath them: conversation and message identity is workspace-level
(`workspace_id`, `channel_id`, `message_ts`), so where the two observers'
visibility overlaps, their observations land on one document rather than two.
The minted scopes govern authorization, enrollment, and provenance: each
document records which enrolled sources observed it, and a search hit is one
message regardless of how many observers cover its channel. Deleting or
unenrolling one observer never removes a document another enrolled observer
still covers.

Depends on the connector-class work tracked as `gap-0378c305`.

### 4.3 Corpus-side landing

Landed records feed the existing transcript projection so exactly one code path
builds conversation documents. Recommended shape: add a landed-record storage
variant to `TranscriptStorage` plus a Slack arm to `TranscriptSource` so a
collected-style adapter serves the existing `TranscriptReadAdapter` contract.
The adapter contract is currently path-oriented (locations carry a filesystem
path, and the cursor store fingerprints locations by path), so this needs a
non-path location fingerprint: a contained change, worth paying to avoid a
second projection path that will drift. The alternative, keeping Slack outside
the registry and driving the normalized-event projection directly, buys nothing
except that drift.

Slack messages index with their own source label so `bbox_search` can include
or exclude them with one filter. Authorship does not fit the transcript role
vocabulary, which describes turn kind rather than identity, so the author id
rides a dedicated indexed field and the role lane collapses to human-versus-app
purely to keep existing role filters meaningful. Callers who care who spoke
filter on author, not role.

## 5. Observation model

### 5.1 Backfill

Initial ingestion is a bounded sweep with an operator-configured horizon: a
start date, a message count, or explicitly "all". A full-history sweep of a
busy workspace is a large rate-limited operation and must be a choice, not a
default. Backfill runs in its own budgeted lane so it cannot starve steady
state, and it resumes from the server's recorded oldest-observed mark.

### 5.2 Steady state

Per-channel `ts` watermark. Each cycle polls `conversations.history` with
`oldest` set to the watermark and pages forward. New messages land as a batch;
the server advances the cursor only after the batch is durable.

### 5.3 Thread completeness

Threaded replies do not appear in `conversations.history` unless they were also
broadcast to the channel. A history-only connector silently loses most thread
bodies, which is the easiest way to ship an ingestion lane that looks correct
and is not. The connector therefore tracks thread parents (any message with a
reply count) and sweeps `conversations.replies` per parent with its own
watermark. A parent's latest-reply timestamp is the cheap test for whether a
resweep is needed, so idle threads cost nothing.

### 5.4 Edit and delete reconciliation

Slack exposes no "changed since" query. An edited message returns from a sweep
with its original `ts` and a new edit timestamp; a deleted message is simply
absent. Both are invisible to a forward-only cursor.

The connector maintains a bounded **reconciliation window**: a trailing span of
each channel (a duration or message count) re-enumerated on a slower cadence
and diffed against what the corpus holds. A changed edit timestamp emits an
update record; an absence emits a tombstone. The corpus applies both against
the stable `(channel_id, message_ts)` identity, so an edit updates in place
rather than duplicating.

The honest limit: **delete detection is window-bounded.** A message deleted
outside the window is not detected, and the corpus may retain text the
workspace no longer shows. That is a policy fact the operator accepts when
enabling the connector; it is why the window is configurable and why the hint
channel matters. Stating the limit beats claiming coverage that does not exist.

The hint channel: where the bridge runs on the same host, it can hand the
reader `message`, `message_changed`, and `message_deleted` notices as
low-latency hints that mark a channel dirty and pull its reconciliation window
forward immediately. Hints are advisory. Nothing lands or is removed on a hint
alone; every hinted change is confirmed by an API read.

### 5.5 Rate-limit discipline

`conversations.history` and `conversations.replies` sit in Slack's lower-tier
per-method, per-workspace bands with small burst allowances, and the thread
sweep multiplies call count by thread cardinality rather than message count.
The producer runs one token bucket per workspace credential, honors
`Retry-After` on every 429 without exception, and never parallelizes across
channels beyond the workspace budget. Lane priority is steady state, then
reconciliation, then backfill. A throttled producer shows up corpus-side as lag
on the status endpoint, not as errors.

## 6. Scope, visibility, and privacy

**RULED (operator, 2026-08-14): membership-driven enrollment is a named
config mode.** The default posture stays explicit per-conversation
enrollment (an empty include enrolls nothing). A deployment whose bot has
deliberately narrow membership may set `enrollment = "membership"`: every
member channel of an enabled class enrolls, an invite is enrollment, a
non-empty include still narrows, and excludes still win. This never widens
visibility beyond the membership bound; it removes the need to restate that
bound in globs. The first deployment runs membership mode.

- **Index only what the token can already see.** The connector never widens its
  own visibility: it never calls `conversations.join`, never enrolls a channel
  because it was mentioned or linked, and never follows a Slack Connect link
  into another workspace.
- **Explicit operator allowlist.** Enrollment is two-sided operator config: the
  channel in the producer host's config, its scope in the corpus host's
  producer grant. A denylist overrides the allowlist. Same shape as remote
  project onboarding, for the same reason.
- **No agent self-service enrollment.** The bridge design floated a
  `/bbox index-channel` slash command for per-channel opt-in. Retired here: it
  is a user-triggered mutation of corpus scope, the exact shape the onboarding
  design rejected.
- **Private channels and DMs are policy-gated, default off.** Each class is a
  separate flag, and enabling a class still requires per-conversation
  allowlisting. DMs are never allowlisted by pattern.
- **Read-only, mechanically.** The producer's Slack grant carries no write
  scope; the producer asserts this at startup and refuses to run otherwise. The
  operator's standing no-writes-to-Slack rule is untouched by this design, and
  this connector could not post if instructed to.
- **Unenrollment purges, per remaining coverage.** Removing a channel from a
  source's allowlist removes that source's observation claim; the channel's
  documents are purged when no enrolled source still covers them, and remain
  searchable (with updated attribution) while another observer does. A channel
  no enrolled source covers must stop being searchable.

## 7. Retrieval

Messages surface through `bbox_search` and hybrid search as conversation
documents carrying indexed provenance: workspace id, channel id, observed
channel name, thread parent ts, author id, observed display name, message ts,
and permalink.

Permalinks are **derived at index time** from workspace, channel, and `ts`
rather than fetched. `chat.getPermalink` costs one call per message, which is
unaffordable at corpus scale; the archive URL form is deterministic from fields
the connector already holds, and the API call remains a fallback for shapes the
derivation does not cover.

Slack documents are searchable by default, with the source label available as a
filter. Making privacy-gated conversations opt-in at query time as well as at
ingest time was considered and rejected as the default: two independent privacy
layers that can disagree is a footgun, and the honest control point is what
gets ingested, not what gets returned. If a conversation should not be
findable, it should not be enrolled. The counterargument is in section 10.

**Graph projection is future work.** The bridge design sketched a Slack entity
grammar (`slack_message`, `slack_user`, `slack_channel`, `slack_thread`) with
`IN_THREAD`, `BY_USER`, `IN_CHANNEL`, and permalink-anchored provenance edges.
That sketch is salvage input to the reflective-graph connector program, not to
v1 here (see
[`reflective-graph-connector-program.md`](reflective-graph-connector-program.md)
and [`../corpus/agentic-corpus/reflective-project-graph.md`](../corpus/agentic-corpus/reflective-project-graph.md)).
What v1 owes that projection is field completeness: the provenance fields above
are chosen so a later graph pass can build channel, thread, author, and
permalink vertices from landed records without a re-ingest. Tracked as
`gap-5d57d2bb`; the cross-entity chain that would walk provenance from a code
line through a commit and a session to the motivating Slack message is
`gap-616857f8`.

## 8. Non-goals

- No writes to Slack of any kind: no posting, editing, reactions, joins,
  channel creation, or modals. The bridge owns interaction; this connector owns
  observation.
- No agent self-service channel enrollment, and no MCP tool that mutates
  ingestion scope.
- No full-workspace exfiltration by default. There is no "index everything"
  posture; enrollment is explicit and per-conversation.
- Not an audit or compliance archive. This is a searchable corpus with
  window-bounded delete detection, not a legally defensible record. That need
  is served by Slack's own export and retention machinery.
- No replacement for the bridge's live search context, which remains the
  surface for not-yet-ingested workspace content.
- No chunking, rendering, or summarization in the producer.
- No Slack dependency inside the corpus daemon.

## 9. Phases and gates

**S0. Contract and policy.** Record shape, endpoint family, policy config
schema, read-only scope assertion. Blocked on the connector-scope family from
`remote-source-connectors.md`.
*Gate:* shapes reviewed against the code-source wire; scope discriminant
decided. No code.

**S1. Producer skeleton, steady state only.** One allowlisted public channel,
forward cursor, no threads, no backfill, no reconciliation.
*Gate:* dependency acceptance test passes (no corpus, chunker, index, or
harness crates in the producer tree); a write-scoped Slack token is refused at
startup; a non-loopback plain-HTTP corpus URL is refused; a scope outside the
grant is rejected before any durable write.

**S2. Corpus projection and search.** Landed records project into conversation
documents; `bbox_search` returns them with full provenance and a source filter.
*Gate:* a known message is findable by text and returns a correct permalink;
unenrolling a channel purges its documents; a full reindex from landed records
is deterministic and duplicate-free.

**S3. Threads and backfill.** Per-parent `conversations.replies` sweeps with
their own watermarks; bounded backfill lane with an operator horizon; the
workspace token bucket enforced across all three lanes.
*Gate:* a thread whose replies were never broadcast to channel is fully
indexed; a fixture backfill completes within its declared call budget with zero
unhandled 429s; a saturated backfill demonstrably does not delay steady state.

**S4. Reconciliation.** Trailing-window resweep, revision and tombstone
records, corpus-side apply.
*Gate:* an edited message updates in place and keeps its identity; a message
deleted inside the window disappears from search; a message deleted outside the
window is reported by the status surface as a known accepted limit rather than
silently claimed handled.

**S5 (operator-gated). Private channels and DMs.** Per-class policy flags with
per-conversation allowlisting.
*Gate:* explicit operator config per conversation; the OAuth grant separately
audited; retention posture documented and accepted.

**Future, not phased here.** Attachment bytes through the content-addressed
blob lane (the file lane genuinely fits files, and `url_private` URLs expire on
workspace-side deletion, so bytes must be captured at observation time). Graph
projection under the connector program.

## 10. Open questions

1. **Non-git scope minting.** Inherited from the connector family; see
   `remote-source-connectors.md`. Not re-decided here.
2. **One Slack app or two.** Recommendation is two. Operator call, because it
   is a workspace-admin act.
3. **Role mapping.** Collapsing authorship to human-versus-app to preserve
   existing role filters is pragmatic, not obviously right. A dedicated
   author-kind field may be cleaner than reusing role.
4. **Default search inclusion for privacy-gated conversations.** Section 7
   makes everything ingested searchable. The counterargument, that a private
   channel ingested for one purpose should not silently widen every agent's
   recall, is real and not fully answered.
5. **Reconciliation window size,** and whether documents older than the window
   should carry a staleness marker so readers know delete detection no longer
   covers them.
6. **Hint-channel coupling.** Whether the bridge-to-reader hint channel earns
   its coupling early or should wait until reconciliation lag is measured.
7. **Retention conflict.** A workspace with an auto-delete retention policy
   expects messages to vanish; an ingested corpus outlives that policy, and
   window-bounded delete detection will not close the gap. Whether the
   connector should mirror a workspace retention setting as a corpus TTL is
   unresolved and organizationally weighty.

## 11. Relationship

- **Extends** [`../daemon-runtime/distributed-code-source-collector-impl.md`](../daemon-runtime/distributed-code-source-collector-impl.md):
  adopts its producer template (dependency-clean satellite, server as cursor
  authority, `ServiceToken` grants bound to durable scopes, bounded bodies, no
  spool) and applies it to an append-only API dataset instead of a file tree.
- **Extends** [`../daemon-runtime/locality-first-decomposition.md`](../daemon-runtime/locality-first-decomposition.md):
  Slack is a producer-plane observation, and the split it draws between
  current-state code shipping and append-only transcript shipping is the
  argument of section 2.
- **Companion of** [`remote-source-connectors.md`](remote-source-connectors.md):
  the file-tree profile of the same family, and owner of the non-git
  scope-minting analysis inherited here.
- **Companion of** [`../integrations/slack/bro-slack.md`](../integrations/slack/bro-slack.md)
  and [`../integrations/slack/bro-slack-next.md`](../integrations/slack/bro-slack-next.md):
  the bridge owns interaction, this connector owns observation. They share a
  workspace, a producer host, and a credential plane, and nothing else.
- **Continues** the bridge's deferred Phase II ingestion sketch (channel
  indexing, file ingestion, permalink-anchored provenance, Slack entity refs),
  salvaged here and re-homed onto the locality axis; the bridge docs should
  stop claiming them.
- **Companion of** [`../operations/config-artifacts/secrets-provider.md`](../operations/config-artifacts/secrets-provider.md):
  how the producer's Slack credential and corpus `ServiceToken` reach the host.
- **Feeds** [`reflective-graph-connector-program.md`](reflective-graph-connector-program.md):
  graph projection of channels, threads, authors, and permalink provenance is
  program work, not v1 work here.

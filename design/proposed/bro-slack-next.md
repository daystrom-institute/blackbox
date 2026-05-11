# bro-slack next - Slack-native agent iteration

Status: design draft
Date: 2026-05-06
Baseline: `design/bro-slack.md`

## 1. Thesis

The v1 Slack integration made the right architectural choice: Slack is a
transport adapter, not a daemon feature. `bro-slack` owns Socket Mode,
Slack credentials, envelope normalization, reconnect/backoff, and ACL
projection. `blackboxd` owns generic webhook routing, workflow execution,
signals, notes, inbox, and orchestration. Product behavior lives in
workflow JSON.

The next iteration should not undo that. The correction is at the Slack
UX layer: Slack now has native agent surfaces and streaming APIs that fit
long-running bro work better than final-only `chat.postMessage` calls.
The sidecar remains the ingress adapter, but outbound should move from
raw workflow-level Slack HTTP snippets toward a small Slack-aware egress
layer that understands:

- app threads and the AI assistant container
- `assistant_thread_started` and `assistant_thread_context_changed`
- `assistant.threads.*` metadata calls
- `chat.startStream` / `chat.appendStream` / `chat.stopStream`
- `task_update` and `plan_update` stream chunks
- Slack Web API `ok: false` logical failures
- `Retry-After` handling on 429s
- action-token-bound Slack search context

The result should feel like a native Slack agent: users can invoke it from
the split-pane agent container, DMs, channel mentions, shortcuts, slash
commands, and App Home. Long operations show progress and stream output
instead of disappearing into an arc and posting one final blob.

## 2. Sources

Primary Slack docs used for this iteration:

- AI in Slack overview:
  https://docs.slack.dev/ai/
- Agent interaction surfaces and entry points:
  https://docs.slack.dev/ai/agent-entry-and-interaction/
- `chat.startStream`:
  https://docs.slack.dev/reference/methods/chat.startStream/
- `chat.appendStream`:
  https://docs.slack.dev/reference/methods/chat.appendStream/
- `chat.stopStream`:
  https://docs.slack.dev/reference/methods/chat.stopStream/
- `assistant.threads.setStatus`:
  https://docs.slack.dev/reference/methods/assistant.threads.setStatus/
- `assistant.threads.setTitle`:
  https://docs.slack.dev/reference/methods/assistant.threads.setTitle/
- `assistant.threads.setSuggestedPrompts`:
  https://docs.slack.dev/reference/methods/assistant.threads.setSuggestedPrompts/
- `assistant_thread_started`:
  https://docs.slack.dev/reference/events/assistant_thread_started/
- `assistant_thread_context_changed`:
  https://docs.slack.dev/reference/events/assistant_thread_context_changed/
- `assistant.search.context`:
  https://docs.slack.dev/reference/methods/assistant.search.context/
- Slack MCP Server:
  https://docs.slack.dev/ai/slack-mcp-server/

## 3. Corrections to carry forward

These are platform corrections, not optional polish.

1. A Slack app factory is not a bot-token factory. Programmatic app
   creation can compress setup, but real bot tokens still come from app
   installation / OAuth. App-per-bro remains operationally heavy.

2. A bot user does not have multiple real mention aliases. Per-message
   `username` / icon customization is display-only. It does not create
   separate autocomplete identities or mention targets.

3. If native `@name` completion for each bro is required, that is still
   separate bot users/apps. This next iteration explicitly pivots away
   from that concern and optimizes the single app as a native agent.

4. Channel mentions should reply in thread using `thread_ts` if present,
   otherwise the triggering message `ts`. This should be treated as a
   sidecar projection invariant so every workflow receives a stable
   `thread_ts`.

5. Slash commands are explicit command entry points, not conversational
   split-view interactions. Slack recommends immediate ack and ephemeral
   or deferred responses for command acknowledgements.

6. App Home is the persistent dashboard/control plane. It should show
   running arcs, blocked work, recent completions, approvals, and recovery
   actions instead of being treated as a later decorative surface.

7. Modals are the right primitive for structured input and confirmations:
   cosession kickoff, provider/brofile selection, destructive action
   confirmation, rejection rationale, identity claim, and dispatch scope
   request.

8. Message shortcuts are the right primitive for "act on this message" or
   "explain this thread" flows. Do not force users to copy message text
   into `/bbox`.

9. Streaming responses are first-class. `chat.startStream` starts the
   message, `chat.appendStream` appends text or chunks, and
   `chat.stopStream` finalizes it. Raw final-only `chat.postMessage`
   should become the fallback path.

10. Slack streaming chunks are richer than text. `markdown_text` is the
    direct answer stream; `task_update` maps well to bro/tool progress;
    `plan_update` maps to workflow phase titles; `blocks` maps to final
    controls and citations.

11. Slack Web API calls can return HTTP 200 with `ok: false`. The current
    workflow pattern of `expect_status: [200]` is insufficient for Slack
    correctness.

12. 429 handling needs response headers. Slack's rate-limit retry time is
    communicated through `Retry-After`; hiding headers inside
    `HttpFetchResult` makes correct retry impossible.

13. `assistant.threads.setStatus` is transient. Slack clears status when
    the app replies, and status also expires; it should be refreshed for
    long-running work or explicitly cleared with an empty status when no
    reply follows.

14. `assistant.threads.setTitle` and
    `assistant.threads.setSuggestedPrompts` require an app with Agents &
    AI Apps enabled and assistant scope support. They are not generic
    channel-thread utilities.

15. `assistant.search.context` is interactive context retrieval, not a
    replacement for blackbox ingestion. Bot-token calls require an
    `action_token`, result pages are limited, and rate limits are designed
    for user-initiated searches, not background indexing.

16. The Slack MCP Server is adjacent. It can help external agents operate
    Slack from MCP-compatible clients, but it does not replace the
    in-Slack app/assistant UX and should not be the core ingress path.

## 4. Target user experience

### 4.1 Agent container

The app is configured as a Slack agent with the Agents & AI Apps feature
enabled. A user can open the agent from the top bar / split pane. On
thread start:

- blackbox receives `assistant_thread_started`
- the sidecar projects the assistant thread channel, thread timestamp,
  user, team, enterprise, and active Slack context
- the workflow publishes context-sensitive suggested prompts
- the workflow may set an initial thread title
- no bro dispatch happens until the user sends a message or clicks a
  prompt

On user messages in the app thread:

- the workflow sets status immediately
- a stream is started
- bro work streams progress through `task_update` chunks
- assistant text streams through `markdown_text`
- final citations / buttons arrive in `chat.stopStream`
- App Home is updated if the work created or changed an arc

On `assistant_thread_context_changed`:

- the sidecar updates thread ambient context
- workflows do not automatically dispatch
- the next user message receives current channel context as an input
- if the app cannot access the channel, the prompt says so rather than
  pretending to know it

### 4.2 Channel mentions

In channels, the interaction stays Slack-native and collaborative:

- user writes `@bbox can you triage this?`
- sidecar projects `thread_ts = event.thread_ts ?? event.ts`
- routing starts or resumes an arc keyed by `{team_id, channel, thread_ts}`
- response is in-thread
- short tasks may use `chat.postMessage`
- anything nontrivial uses `chat.startStream` in the thread
- progress appears as stream chunks or periodic threaded updates
- destructive actions require Block Kit buttons or modals

Channel mentions should not use slash-command response URLs. They are
normal channel messages with threaded replies.

### 4.3 Direct messages

DMs are conversational but outside the split-pane app container. Treat
`message.im` as an entry point:

- `channel_type == im` starts/resumes a DM conversation arc
- stream replies in the DM thread where Slack supports it
- fall back to normal threaded message replies if assistant app-thread
  affordances are unavailable
- suggested prompts belong in the assistant container, not in ordinary
  DM messages unless rendered as Block Kit buttons

### 4.4 Slash commands

Slash commands are explicit task launchers:

- `/bbox status`
- `/bbox inbox`
- `/bbox triage <target>`
- `/bbox explain <ref>`
- `/bbox cosession start`
- `/bbox claim`

Rules:

- ack immediately in the sidecar / Slack handler path
- default acknowledgement is ephemeral
- use `response_url` for deferred command results only when the result is
  tightly tied to the command invocation
- use modals for forms and confirmations
- use App Home links/buttons for dashboards and long-running control
- do not make slash commands the primary conversational surface

### 4.5 App Home

App Home becomes the operator console:

- active arcs started from Slack
- active non-Slack arcs relevant to the mapped bbox user
- blocked items / unresolved inbox entries
- approvals waiting for the user
- recent completions
- buttons: stop, retry, resume, view thread, open proposal, claim identity
- settings: default provider, preferred brofile, dispatch scopes, indexed
  channels

`app_home_opened` should render immediately. If building the full view is
expensive, publish a small loading view, compute state, then publish the
final view.

### 4.6 Message shortcuts

Message shortcuts should be added for message-scoped actions:

- "Ask bbox about this"
- "Summarize this thread"
- "Create proposal from this"
- "Explain provenance"
- "Add to blackbox memory"

The shortcut payload gives the exact message context. Use `views.open`
when additional fields or confirmation are needed. Route the submitted
modal into a workflow with the original Slack message reference attached.

### 4.7 Link unfurls

Slack unfurls can make blackbox references shareable:

- `bbox://thread/...`
- `bbox://arc/...`
- local web UI links if blackbox grows one
- permalink citations from Slack ingestion

Subscribe to `link_shared` only for domains / URL schemes blackbox owns.
Call `chat.unfurl` with compact Block Kit: title, status, one-line
summary, and actions.

## 5. Architecture

### 5.1 Keep the sidecar boundary

The boundary from v1 remains:

```text
slack.com
  -> bro-slack sidecar
  -> blackboxd /webhook/slack
  -> routing packet
  -> workflow / arc / bro execution
  -> Slack-aware egress
  -> slack.com Web API
```

The daemon still does not need to link a Slack SDK. The sidecar still
does not run workflows. Slack-specific product behavior still belongs in
workflow examples and routing packets.

### 5.2 Add a Slack-aware egress layer

The current v1 pattern puts raw `http_json` Slack calls in workflow JSON.
That is acceptable for proofs but wrong as the permanent API because
Slack has cross-cutting semantics:

- every response body must be checked for `ok`
- 429 requires `Retry-After`
- stream lifecycle is stateful
- status calls should be deduplicated/throttled
- stopStream must run on normal completion and error
- some calls need bot token, some can use response URL
- some calls are bound to assistant threads and scopes
- error bodies need to be surfaced as workflow warnings / failures

Add a Slack egress helper used by workflow ops. Two implementation shapes
are acceptable:

1. `slack_api` as a workflow op:
   - `method`: Slack method name, e.g. `chat.postMessage`
   - `body`: JSON object
   - `token_env`: defaults to `SLACK_BOT_TOKEN`
   - `expect_ok`: defaults true
   - `retry`: built-in 429 and transient retry policy
   - returns `{ ok, status, headers, value, error, retry_after_secs }`

2. Generic `http_json` grows headers and a Slack response validator:
   - `capture_headers: ["retry-after", "x-slack-req-id"]`
   - `expect_json_field: { path: "$.ok", value: true }`
   - reusable beyond Slack, but more verbose in workflows

Bias: implement `slack_api` as a thin wrapper over improved
`HttpFetchResult`. That keeps workflow JSON readable while also fixing
the generic header gap.

### 5.3 Add a high-level stream helper

Streaming through raw workflow JSON will be fragile. Add a helper that
models a stream as a resource:

```jsonc
{
  "op": "slack_stream_start",
  "args": {
    "channel": "${vars.channel}",
    "thread_ts": "${vars.thread_ts}",
    "markdown_text": "Starting...",
    "metadata": {
      "arc_id": "${arc.id}",
      "workflow": "${workflow.name}"
    }
  },
  "into_var": "slack_stream"
}
```

The returned value:

```jsonc
{
  "channel": "C123",
  "ts": "1730816096.000300",
  "thread_ts": "1730816096.000300",
  "active": true
}
```

Follow-up ops:

- `slack_stream_append_text`
- `slack_stream_append_chunks`
- `slack_stream_task_update`
- `slack_stream_plan_update`
- `slack_stream_stop`
- `slack_stream_abort`

The helper must enforce:

- chunk coalescing to avoid excessive API calls
- per-stream append ordering
- stop exactly once
- best-effort stop on workflow failure
- final fallback `chat.postMessage` if startStream is unavailable
- preserving Slack request IDs for debugging

### 5.4 Stream source: node-level first, token-level later

Do not block the next iteration on provider-token streaming. There are
two levels:

1. Node-level streaming:
   - start a stream when an arc begins
   - append task updates on workflow phase changes and bro lifecycle
     events
   - append the final bro answer at completion
   - stop with final blocks / citations

2. Token-level / text-delta streaming:
   - consume provider streaming events or bro tail text deltas
   - append incremental markdown text to Slack
   - coalesce deltas by time and size
   - stop when final assistant text is known

Node-level streaming is enough to feel alive and is much easier to make
correct. Token-level streaming can land after the egress helper and
workflow failure semantics are stable.

### 5.5 Event normalization v2

Extend the normalized envelope with AI-agent fields while preserving the
v1 top-level fields.

New projected fields:

```jsonc
{
  "type": "events_api",
  "event_type": "assistant_thread_started",
  "team_id": "T123",
  "enterprise_id": "E123",
  "user": "U123",
  "channel": "D123",
  "thread_ts": "1729999327.187299",
  "assistant_channel_id": "D123",
  "assistant_thread_ts": "1729999327.187299",
  "context_channel_id": "C123",
  "context_team_id": "T123",
  "context_enterprise_id": "E123",
  "event_ts": "1715873754.429808",
  "action_token": null,
  "raw_event": { "...": "..." }
}
```

Rules:

- For `assistant_thread_started`, `user` is
  `assistant_thread.user_id`.
- `channel` is the assistant thread channel.
- `thread_ts` is the assistant thread timestamp.
- `context_*` fields come from `assistant_thread.context` if present.
- For `assistant_thread_context_changed`, update the same fields and
  route as a context signal, not a new task.
- For `message.im`, preserve `action_token` when Slack includes one so
  `assistant.search.context` can be called with a bot token.
- For app mentions, keep `thread_ts = event.thread_ts ?? event.ts`.
- For slash commands, keep `response_url`, `trigger_id`, `command`, and
  `command_text`.
- For interactive payloads, keep `trigger_id`, `action_id`,
  `action_value`, `view_id`, `view_state_values`, and raw body.

### 5.6 Routing packet additions

Add rules for:

- `assistant_thread_started` -> start lightweight assistant bootstrap
- `assistant_thread_context_changed` -> signal/update context for an
  existing assistant arc
- `message.im` in assistant/DM channels -> start or signal a
  conversational arc
- `app_home_opened` -> start App Home render workflow
- message shortcuts -> start message-scoped workflow
- global shortcuts -> open modal or start workflow
- `link_shared` -> unfurl if the link belongs to blackbox

Routing should distinguish "start a real bro task" from "render/update
Slack UI". Assistant thread start and App Home open are mostly UI events;
they should not dispatch expensive bro work by default.

### 5.7 Correlation model

Use a single Slack conversation key everywhere:

```text
slack_conv = team_id + ":" + channel + ":" + thread_ts
```

For assistant threads, `channel` is the assistant channel and
`thread_ts` is the assistant thread timestamp. The active Slack context
is separate:

```text
context = team_id + ":" + context_channel_id
```

Do not conflate the assistant thread channel with the channel the user
was viewing. The assistant thread is where the conversation lives; the
context channel is optional grounding input.

`start_arc` idempotency keyed on `(workflow, slack_conv)` remains a v2
prerequisite for correctness. Without it, duplicate Slack events or two
near-simultaneous mentions can still start duplicate arcs.

### 5.8 State storage

Add a small Slack runtime state store, either daemon-side or sidecar-side
depending on where egress lands:

```jsonc
{
  "team_id": "T123",
  "channel": "D123",
  "thread_ts": "1729999327.187299",
  "arc_id": "arc-...",
  "workflow": "slack-agent-answer",
  "stream_ts": "1729999329.000100",
  "last_context_channel_id": "C123",
  "last_status": "is reading channel context...",
  "last_status_at": "2026-05-06T12:00:00Z",
  "created_at": "2026-05-06T12:00:00Z",
  "updated_at": "2026-05-06T12:00:20Z"
}
```

This store must not become a second workflow engine. It exists to make
Slack egress idempotent and debuggable:

- stream started?
- status currently set?
- final stop sent?
- context updated?
- which arc owns this Slack thread?

If the workflow engine grows durable WaitStore and arc idempotency, this
state can be a projection of arc state rather than independent state.

## 6. Slack API usage by surface

### 6.1 Agent container / app threads

Inbound:

- `assistant_thread_started`
- `assistant_thread_context_changed`
- `message.im`

Outbound:

- `assistant.threads.setSuggestedPrompts`
- `assistant.threads.setTitle`
- `assistant.threads.setStatus`
- `chat.startStream`
- `chat.appendStream`
- `chat.stopStream`

Notes:

- suggested prompts should be dynamic, using user, channel, recent
  blackbox state, and active context
- title should be set from the user's first meaningful prompt and refined
  after intent classification
- status should be short and action-oriented
- status should not be updated for every token or tiny event

### 6.2 Channel mentions

Inbound:

- `app_mention`
- optional `message.channels` / `message.groups` / `message.im` for
  thread follow-up if the app is expected to continue without repeated
  mentions

Outbound:

- `chat.startStream` for nontrivial answers
- `chat.postMessage` fallback
- `chat.update` for proposal status
- Block Kit buttons for approvals
- `views.open` for rationale / confirmation

Notes:

- always thread replies
- avoid dumping private/in-progress diagnostic detail into channel
- use ephemeral messages for permission failures or "I need more input"

### 6.3 Slash commands

Inbound:

- slash command payload through Socket Mode

Outbound:

- immediate ack
- `response_url` for deferred command result
- `chat.postEphemeral` for private acknowledgement/errors
- `views.open` for structured input
- App Home publish for persistent dashboards

Notes:

- slash commands are not supported in app threads
- default command response should be ephemeral unless the command
  explicitly produces a shared artifact
- long-running commands should create/update an App Home item and post a
  thread link if there is a visible channel context

### 6.4 App Home

Inbound:

- `app_home_opened`
- Block Kit actions from App Home controls

Outbound:

- `views.publish`
- `views.update` only for modals, not Home

Notes:

- App Home should be cheap to render from stored state
- do not run heavy retrieval synchronously before the first publish
- controls should map to explicit signals: stop, retry, resume, approve,
  reject, claim identity, request dispatch scope

### 6.5 Modals and shortcuts

Inbound:

- message shortcuts
- global shortcuts
- block actions
- view submissions

Outbound:

- `views.open`
- `views.update`
- `chat.postEphemeral`
- `chat.postMessage` / stream after submission

Notes:

- prefill modal fields from Slack context
- use `private_metadata` to carry compact correlation IDs
- do not encode large payloads in `private_metadata`; store server-side
  and reference by ID

### 6.6 Slack search context

Inbound requirement:

- user-initiated action carrying an `action_token`, or user-token auth

Outbound:

- `assistant.search.context`

Use cases:

- "what is the latest in this channel?"
- "summarize the decision in the thread I am looking at"
- "find mentions of project X from this week"

Rules:

- call less than 10 times per user inquiry
- request context messages when useful
- cite `permalink` values in answers
- prefer blackbox's own index for durable/project knowledge
- use Slack search for live workspace context that blackbox has not
  ingested
- do not use it for background indexing

## 7. Workflow design changes

### 7.1 Split workflows by surface

Do not stretch one `slack-badgey-ask` workflow over every Slack surface.
Create separate workflows with explicit contracts:

- `slack-agent-thread-start`
- `slack-agent-answer`
- `slack-channel-mention`
- `slack-dm-answer`
- `slack-command`
- `slack-app-home-render`
- `slack-message-shortcut`
- `slack-modal-submit`
- `slack-unfurl`

Shared behavior should be helpers, not hidden prompt convention.

### 7.2 Standard workflow vars

Every Slack workflow should receive:

```jsonc
{
  "team_id": "...",
  "enterprise_id": "...",
  "channel": "...",
  "thread_ts": "...",
  "user": "...",
  "bbox_user": "...",
  "bbox_scopes": ["..."],
  "bbox_can_dispatch": true,
  "surface": "agent_thread|channel_mention|dm|slash|home|shortcut|modal",
  "response_url": null,
  "trigger_id": null,
  "action_token": null,
  "context_channel_id": null,
  "raw_event_ref": "delivery-or-event-id"
}
```

Keep raw Slack payloads out of ordinary prompts. Preserve raw data for
debugging and extraction; pass concise projected fields to actors.

### 7.3 Standard egress policy

Each workflow declares:

```jsonc
{
  "slack": {
    "reply_mode": "stream|message|ephemeral|response_url|home",
    "visibility": "public|thread|private",
    "status": true,
    "stream_progress": "node|token|none",
    "final_blocks": true
  }
}
```

This can be metadata at first, used by examples and tests. Later it can
drive default egress behavior in the workflow engine.

### 7.4 Prompt changes

Prompts should no longer say only "Use Slack-compatible mrkdwn." They
should tell the actor:

- the Slack surface
- whether the reply is public, threaded, ephemeral, or app-thread only
- whether destructive actions are allowed
- whether Slack live search context is available
- whether citations/permalinks should be included
- whether the answer will be streamed

Do not ask the model to produce Block Kit JSON unless the workflow is
explicitly a UI-rendering workflow. Ordinary answer workflows should
return structured answer text plus optional action/citation records; the
egress layer renders Slack.

### 7.5 Structured outputs for Slack replies

For nontrivial flows, prefer a structured actor output:

```jsonc
{
  "title": "Short thread title",
  "answer_markdown": "...",
  "status_summary": "Done",
  "citations": [
    { "label": "Slack thread", "url": "https://..." },
    { "label": "blackbox session", "ref": "session:..." }
  ],
  "actions": [
    { "kind": "approve", "label": "Apply proposal", "value": "proposal-..." },
    { "kind": "reject", "label": "Reject", "value": "proposal-..." }
  ]
}
```

The Slack renderer turns this into text, stream chunks, final blocks, and
buttons. This avoids making every bro learn Slack Block Kit details.

## 8. Implementation plan

### Phase 0: documentation and manifest update

- Add this design.
- Update `examples/slack/manifest.yaml` to include optional Agents & AI
  Apps notes, assistant events, App Home, shortcuts, and streaming/API
  scope comments.
- Add example routing rules for assistant events but leave them disabled
  until the sidecar projection lands.

### Phase 1: safer Slack egress

- Extend `HttpFetchResult` to expose selected headers.
- Add logical JSON success validation (`ok == true`) or the `slack_api`
  wrapper.
- Capture `x-slack-req-id` and `retry-after`.
- Implement 429 retry using `Retry-After`.
- Surface Slack `ok:false` as a workflow hook failure with the Slack
  error code in the warning/failure event.
- Migrate existing example workflows from raw `http_json` Slack calls to
  `slack_api` or the enhanced generic shape.

Acceptance:

- a simulated 200/`ok:false` fails the node
- a simulated 429 retries after the header delay
- workflow status shows Slack method, Slack error code, and request ID

### Phase 2: assistant event projection

- Extend `src/slack_bridge.rs` normalization for:
  - `assistant_thread_started`
  - `assistant_thread_context_changed`
  - `app_home_opened`
  - message shortcuts
  - global shortcuts
  - `link_shared`
- Add top-level projected fields from section 5.5.
- Extend `examples/slack/webhooks/slack.json` extractor.
- Extend `examples/slack/packets/routing-slack.json`.
- Add replay fixtures for every new event shape.

Acceptance:

- replaying assistant thread start routes to bootstrap workflow
- context change signals existing assistant arc or records context
- app home open routes to home render workflow
- shortcut payloads preserve message reference and trigger ID

### Phase 3: App Home control plane

- Add `slack-app-home-render` workflow.
- Render active arcs, blocked inbox items, approvals, and recent done.
- Add App Home actions:
  - stop arc
  - retry failed Slack post
  - open Slack thread
  - approve/reject
  - claim identity
- Add modal for identity claim / dispatch scope request.

Acceptance:

- opening App Home shows current blackbox state for mapped user
- buttons emit signals with stable correlation IDs
- unmapped users see read-only/claim state

### Phase 4: node-level streaming

- Add stream helper ops.
- Start stream at beginning of long answer workflows.
- Append task updates on workflow phase / bro lifecycle transitions.
- Stop stream with final answer and blocks.
- Fallback to `chat.postMessage` if streaming fails before a stream `ts`
  exists.
- If streaming starts but append fails repeatedly, stop with an error
  summary or post a fallback threaded message.

Acceptance:

- long bro dispatch shows visible progress in Slack
- workflow failure stops the stream with an error state
- duplicate stop attempts are harmless
- final answer includes citations/actions after streamed text

### Phase 5: assistant thread polish

- Enable Agents & AI Apps in manifest/setup docs.
- On `assistant_thread_started`, set dynamic suggested prompts.
- On first user message, set title.
- During long work, set/refresh status at coarse boundaries.
- On completion, clear status if no final message is sent.

Acceptance:

- app thread appears in Slack History with useful title
- suggested prompts are contextual, not static boilerplate
- status does not spam or go stale during long arcs

### Phase 6: Slack live context

- Preserve `action_token` from message events when Slack provides it.
- Add an optional `slack_search_context` helper wrapping
  `assistant.search.context`.
- Enforce per-user/per-inquiry call budgets.
- Return normalized citations with Slack permalinks.
- Merge Slack live context with blackbox search evidence in actor prompts.

Acceptance:

- a Slack-context question can cite Slack permalinks
- rate budget prevents repeated paginated calls in one inquiry
- absence/expiry of `action_token` degrades to blackbox-only context

### Phase 7: token-level streaming

- Connect provider/bro streaming output to Slack append coalescer.
- Coalesce by time and byte threshold.
- Preserve final answer reconciliation: the final stored assistant output
  is authoritative; streamed deltas are UI.
- Avoid leaking chain-of-thought or tool internals.

Acceptance:

- visible answer text streams progressively
- final Slack message matches sanitized final output
- internal tool chatter appears only as `task_update` when explicitly
  user-safe

## 9. Manifest and scopes

Baseline v1 scopes remain:

- `app_mentions:read`
- `chat:write`
- `commands`
- `reactions:read`
- `users:read`
- relevant history scopes for event subscriptions (`channels:history`,
  `groups:history`, `im:history`, `mpim:history`) selected by deployment

Next iteration additions:

- Agents & AI Apps feature enabled in app settings
- App Home tab enabled
- interactivity enabled
- event subscriptions:
  - `assistant_thread_started`
  - `assistant_thread_context_changed`
  - `message.im`
  - `app_home_opened`
  - `app_mention`
  - relevant message events for chosen surfaces
  - `link_shared` if unfurls are enabled
- assistant methods:
  - `assistant:write` for title/suggested prompts
  - `chat:write` for status and streaming
- optional live search:
  - bot token: `search:read.public`, `search:read.files`,
    `search:read.users` as needed
  - user-token path can add private/DM search scopes, but that requires
    a fuller OAuth/user-token design and is not v2 default
- optional council/persona display:
  - `chat:write.customize`
  - `groups:write` or `channels:manage` only if Slack-created council
    channels are actually shipped

Keep least privilege by phase. Do not add search or channel-management
scopes to the default manifest until the corresponding workflows are
enabled.

## 10. Security and privacy

### 10.1 Token boundaries

Keep v1 token isolation:

- app token in sidecar
- signing secret in sidecar
- bot token in daemon or egress process
- optional sidecar-to-daemon HMAC secret on both

If `slack_api` becomes daemon-side, bot token remains daemon-side. If a
separate Slack egress sidecar is introduced later, bot token can move
there, but that is a larger architecture change and not required.

### 10.2 Visibility defaults

- permission failures: ephemeral
- command acknowledgement: ephemeral
- final answer to channel mention: thread reply
- App Home state: per-user
- destructive approval: visible where the proposal is visible, but the
  rationale modal response can be private unless explicitly posted
- live Slack search context: cite sources, do not silently quote private
  context into public channels

### 10.3 Context access checks

Assistant thread context includes the channel the user is viewing, but
the app may not have access to it. Before using context channel content,
check access through Slack APIs or restrict to IDs already available from
the event. If the app lacks access, say that the channel is not available
instead of fabricating context.

### 10.4 Search context rules

`assistant.search.context` can search across Slack data according to
token/user permissions. Treat it as sensitive:

- only call after user-initiated actions
- budget calls tightly
- log query metadata and result counts, not full private result bodies
  unless the deployment explicitly opts into Slack ingestion logs
- cite permalinks in answers when content is used
- do not persist raw Slack search results into blackbox knowledge without
  an explicit ingestion policy

### 10.5 Streaming hygiene

Streaming must never leak:

- hidden reasoning
- tool credentials
- raw exception payloads with secrets
- unreviewed diffs for private repos into public channels
- provider debug logs

Map internal events to user-safe `task_update` summaries. The final
assistant output remains the authoritative answer.

## 11. Observability

Add Slack egress telemetry:

- Slack method
- Slack request ID
- HTTP status
- Slack `ok`
- Slack error code
- retry count
- retry-after seconds
- stream channel/ts
- stream lifecycle state
- arc/workflow/node correlation

Add sidecar ingress telemetry:

- event type
- assistant thread channel/thread
- context channel
- action token present/absent, never token value
- route verdict
- delivery ID

Add debug tools or filters:

- recent Slack deliveries by event type
- recent Slack egress calls by arc
- active Slack streams
- orphan streams not stopped
- App Home render failures
- search context budget exhaustion

The existing generic webhook replay tool remains useful. Add replay
fixtures rather than Slack-specific daemon tools unless a repeated debug
workflow proves otherwise.

## 12. Failure modes

| Failure | Required behavior |
|---|---|
| `chat.startStream` fails before `ts` | Fall back to `chat.postMessage` or ephemeral error. |
| `chat.appendStream` fails transiently | Retry with backoff; coalesce missed text if possible. |
| `chat.appendStream` fails permanently | Stop stream with error if possible; otherwise post fallback reply. |
| workflow fails after stream start | Stop stream with `task_update` error and concise failure summary. |
| process dies after stream start | Recovery job finds active stream state and posts/stops best-effort. |
| 200/`ok:false` | Treat as logical failure, not success. |
| 429 | Retry after `Retry-After`; surface if retry budget exhausted. |
| missing assistant feature | Degrade to channel/DM message mode and log setup warning. |
| missing assistant scope | Disable title/prompts/status, keep core replies. |
| missing action token | Skip Slack live search; use blackbox search only. |
| search rate limited | Stop Slack search for the inquiry; cite that live Slack context was skipped. |
| duplicate event delivery | Header dedup plus `start_arc` idempotency prevents duplicate arcs. |
| duplicate stream stop | No-op after first successful stop. |

## 13. Non-goals

- multiple mentionable bot identities from one Slack app
- hidden/manual bypass of Slack app approval
- replacing blackbox ingestion with Slack live search
- replacing in-Slack app UX with Slack MCP Server
- making the daemon Slack-native internally
- shipping Enterprise Grid org-wide admin automation in this iteration
- broad channel history indexing without a separate privacy design

## 14. Open questions

1. Should `slack_api` be a workflow op, or should `http_json` gain enough
   generic validation/retry features that Slack can remain pure JSON?
   Bias: both, with `slack_api` as a small wrapper.

2. Where should stream state live: workflow vars only, daemon runtime
   store, or sidecar/egress store? Bias: daemon runtime store keyed by
   arc and Slack conversation; later persist if recovery proves necessary.

3. Should node-level streaming ship before App Home? Bias: egress safety
   first, App Home second, streaming third. App Home gives control over
   long-running work even before streaming text lands.

4. Should live Slack search use only bot-token/action-token calls at v2,
   or add user-token OAuth? Bias: bot-token/action-token only. User-token
   OAuth changes security and storage substantially.

5. Should one-app-per-persona remain a future council option? Bias: yes,
   but keep it out of the next iteration. `chat:write.customize` plus
   clear persona labels is enough for council display experiments.

6. How much Slack-specific state should become first-class
   agentic-corpus entities? Bias: only after the Slack UX iteration is
   stable; live app UX and durable corpus ingestion are separate arcs.

## 15. Recommended next cut

The smallest valuable next cut:

1. Add Slack egress correctness:
   - selected response headers
   - `ok == true` validation
   - 429 retry-after handling
   - Slack request ID logging

2. Add assistant event projection:
   - `assistant_thread_started`
   - `assistant_thread_context_changed`
   - `app_home_opened`

3. Add one App Home workflow:
   - active arcs
   - inbox/blocked summary
   - identity claim affordance

4. Add node-level streaming to `slack-channel-mention`:
   - start stream
   - task updates
   - final answer
   - stop stream

That cut keeps the v1 sidecar architecture, fixes the biggest Slack API
correctness holes, and makes the user-visible experience meaningfully more
native without requiring token-level provider streaming or a new identity
model.

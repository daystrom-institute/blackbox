# Transcript Retrieval

Use transcript tools when the answer lives in prior agent conversations: who
said a rule, what a task did, where a failed attempt happened, or what context a
handoff lost.

Use graph tools when the answer depends on connected entities. Use transcript
tools when you need the conversation itself.

## Retrieval Ladder

Start broad:

```text
bbox_hybrid_search(query="redis locking", project="/repo/x", role="user")
```

Open context around a hit. Each transcript hit carries a `conversation` object;
its `exact_read` is a ready `bbox_context` call, or fill one in from the hit:

```text
bbox_context(file_path="<conversation.file_path>", byte_offset=<conversation.byte_offset>)
```

Inspect the session:

```text
bbox_session(session_id="<conversation.session_id>")
bbox_messages(session_id="<conversation.session_id>", from_end=true, limit=40)
```

If you need the origin of a standing claim, search user turns for the quoted
phrase:

```text
bbox_hybrid_search(query="\"never kill processes by port\"", role="user")
```

## Search

`bbox_hybrid_search` is for topics when you do not know the session. It covers
transcripts, tool calls, project files, commits, knowledge and threads; narrow
it to conversations with `doc_type` or any conversation filter:

```text
bbox_hybrid_search(
  query="atom binding workflow",
  project="/repo/x",
  role="assistant",
  exclude_self=true,
  limit=10
)
```

Default smart mode broadens adjacent terms for recall. Use quoted phrases for
exact text and `-term` for exclusions. Use `mode="fulltext"` only when you want
raw Tantivy/Lucene boolean syntax (conjunction by default). `limit` defaults to
10 and caps at 50.

Filter early. Filters apply to every ranking lane (BM25, vectors, knowledge)
before results are ranked:

| Filter | Use |
|---|---|
| `doc_type` | One document family: `transcript`, `tool_call`, `project_file`, `commit`, `knowledge`, `thread`, ... |
| `project` | Keep results scoped to the repo you care about. Transcripts and tool calls scope by recorded working directory or base project when the search is narrowed to conversations. |
| `role` | Find user directives, assistant summaries, tool results, or thinking blocks. |
| `account` | Separate multiple Claude/Codex accounts. |
| `include_subagents` | Include or exclude subagent transcript blocks (default true). |
| `exclude_self` | Avoid echoing the current turn (default false). |
| `source` | Include or exclude lanes, comma-separated (`slack`, `-slack`, `glm,codex`, ...). |
| `author` | Conversation documents: who spoke, by provider user id. |
| `channel` | Conversation documents: one Slack channel by name or id. |

The response is JSON. Each entry in `results[]` has `entity_id`, `label` and
`excerpt`; transcript and tool-call hits add a `conversation` object with
`session_id`, `file_path`, `byte_offset`, `timestamp`, `account`, `source`,
`project`, `author`, `channel`, `permalink`, and `exact_read` when an indexed
recovery handle exists. `next_steps` lists the follow-up readers
(`bbox_context`, `bbox_messages`, and for Slack a channel-filtered
`bbox_hybrid_search`).

## Conversations (Slack Lane)

Connector-landed Slack conversations are searchable through the same
`bbox_hybrid_search`. [Retained conversation enrollment](conversation-retention.md)
keeps explicitly authorized history readable after removing its ingest grant;
it does not imply collection is current. The channel is a first-class coordinate:

```text
bbox_hybrid_search(query="import mapping", channel="#ops-incident-4565")
bbox_hybrid_search(query="ops-incident-4565", doc_type="transcript")   # channel names match plain queries
```

A `channel` name (leading `#` accepted) resolves through the current roster to
the stable channel id, so a renamed channel still matches its whole history;
documents stamped with the queried name keep matching directly. A channel id
works as-is.

Conversation hits carry channel, author, and a derived Slack permalink in their
`conversation` object. They have no readable transcript FILE, but `bbox_context`
and `bbox_messages` still apply: both resolve a slack hit's coordinates against
the conversation landing store directly instead of reading a file, exactly as
the hit's `next_steps` recommend.

```text
bbox_context(file_path="slack:T4MNK2L6A/C0BP8JG21FU", byte_offset=1712345678000200)
bbox_messages(session_id="C0BP8JG21FU/2026-08-10")
```

`bbox_context`'s `file_path` is the hit's `conversation.file_path`
(`slack:<workspace_id>/<channel_id>`); its `byte_offset` is not a byte position
but the target message's timestamp with the decimal point removed (the same
digit-concatenation the permalink derivation uses), and `conversation.byte_offset`
already carries it. `bbox_messages` pages the whole channel with
`file_path="slack:..."`, or just one day with `session_id="<channel_id>/<date>"`
(the per-channel-per-day bucket in `conversation.session_id`). Either way, an
unenrolled or unknown channel refuses with a message naming a working
`bbox_hybrid_search(channel=...)` lane rather than an ENOENT or "Session not
found". Narrowing `channel=` queries and following the permalink to open a
message in Slack remain the other two working paths. A hit whose only match was
metadata (the channel name, lane, or author) shows the start of the message as
its excerpt.

## Sessions And Messages

Use `bbox_sessions_list` to browse retained sessions by latest indexed activity:

```text
bbox_sessions_list(project="/repo/x", limit=20)
```

Listings group by source, account, and session ID, including collected native
history. `source` selects the provider format; `account` matches the exact
source-owner label. Pages contain at most 100 rows and return `next_offset`;
byte limits can shorten a page. Missing timestamps sort last, with stable
identity ordering for ties. Separate pages can change as indexing advances.
Session names are not retained in this projection, so `name` filters return
`error.session_names_not_indexed` rather than reading host metadata. An empty
page means no matching retained sessions, not that producer history is empty.

Use `bbox_session` for retained message count, source identities, and indexed
time range. Session IDs are exact; no producer file is opened.

Use `bbox_messages` when you already know the session and need chronological
flow. Tail mode is useful for takeover:

```text
bbox_messages(session_id="<session-id>", from_end=true, limit=80)
```

## Reindex And Health

Background reindex runs periodically. Reach for manual tools only when you are
diagnosing freshness:

```text
bbox_stats()
bbox_reindex(full=false)
bbox_embed_status()
```

`bbox_stats` reports indexed document and segment counts, cached up to 60 seconds.
These totals include indexed collected sources and do not depend on transcript
roots being present on the daemon host. The tool does not assess source coverage,
freshness, disk size, or edge totals. Missing local directories or optional edge
state are not evidence of missing source data or zero edges.

Use `full=true` only after schema changes, corruption, or an explicit reason to
throw away incremental assumptions.

## When To Use Graph Instead

Use [Internals](internals.md) and the agentic opening sequence when the question
needs relationships across entities:

- what thread or session produced a commit
- which docs and symbols relate to an artifact

Transcript search finds text. The graph finds paths.


## Native source limits and continuation

Pass the search hit's `conversation.file_path` unchanged: it is an opaque stored
locator, including when it looks like a path. Context and messages never open it
on the daemon. Native replies describe retained indexed projections, which may
already be parser-truncated. For enrolled native sources, `source_freshness`
includes bounded publication and producer observations:
`index_matches_published`, `published_at`, last contact, and completed-scan
failure/deferred counts. Publication time does not establish producer liveness,
and a completed scan with failed or deferred streams does not establish
completeness. Untracked legacy locators report `not_established`. Reindexing
cannot recover history that no producer has delivered; the source owner must
publish/backfill retained files through the configured native transcript
collector.

`bbox_context` selects indexed events around an exact offset (default five on
each side, maximum 25), with short previews. Expand with `bbox_messages` using
exactly one of `session_id` or `file_path`. Follow `next_offset`, including when
`from_end=true`; it accounts for byte-limited pages. Native message content is
capped at 12000 stored bytes even with `max_content_length=0`. Preview truncation
is separate from truncation already present in the stored source projection.

See [Native transcript collection](native-transcript-collector.md) for source
enrollment, backfill, and producer health recovery.
